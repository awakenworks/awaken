# Guard ownership and retirement

## One enforcer per rule

Cargo and the compiler own dependency reachability, visibility and type laws
they can express. Existing semantic checkers retain invariants the compiler
cannot establish. A rule retires only when its replacement covers the same
causes, refusals and effects, including a meaningful negative control.

## Compiler and Cargo boundaries

| Rule reference | Authoritative enforcement and boundary |
| --- | --- |
| R8 | Production visibility is held by Rust visibility lints and explicit module boundaries. |
| R9 | Wildcard imports use the authored Clippy policy and narrowly justified expectations. |
| R11 | Process environment access belongs to the configuration or adapter edge, with authored disallowed-method boundaries. |
| R13 | Clock acquisition belongs to the time or process edge; domain decisions consume explicit clock values. |
| R14 | Storage arithmetic uses typed values and checked conversions; SQL and ownership semantics remain with their existing guards. |
| R15-R17 | Aggregate privacy, compatible DTOs, construction authority and observation translation are different rules; a private field does not replace every semantic guard. |
| R20 | Retired vocabulary uses the existing naming authority rather than repeated tombstone tables. |

`Cargo.toml`, `clippy.toml`, crate metadata and the existing architecture
checkers hold the actual declarations. Do not copy their counts or inventories
here. Optional deployment dependencies remain classified by their semantics.
Do not introduce mirror DTOs, bulk getters, marker traits or compatibility
wrappers merely to reduce a diagnostic count.

## Shared mechanisms

The existing workspace reader owns dependency metadata and Rust item parsing.
Semantic consumers reuse that reader and retain their own bounded rule.
`scripts/ci/hooks.yml` owns authored hook jobs; its existing generator renders
the hook configuration and input closures. Generated files do not acquire a
second hand-maintained source.

The existing test planner owns source selection; the existing execution reader
owns listed-versus-executed evidence and source identity. The formal planner
owns proof selection. Target, temporary-file and process admission helpers own
their existing resource boundaries. Host queues and an individual run's timing
or recovery work are execution records, not repository policy.

## Retirement review

Compare the replacement's legal domain, error behavior and real callers with
the existing enforcer before removing it. Keep semantic ownership and failure
classes intact. Source-size baselines remain diagnostics or their explicitly
authored ratchets; a copied phase inventory is not their source of truth.

See [test design and execution](test-validation.md) for evidence boundaries and
the owning architecture ADRs for component responsibilities.
