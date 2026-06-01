## Core Rules

- Production-ready only; no temporary solutions. Blocked? Open a GitHub issue
- Honor `docs/` specifications; reconcile inconsistencies
- Before destructive ops (`rm`, `git reset`), inspect repo state first
- No process/status documents; only architecture docs
- No estimates or project management info in technical docs
- No placeholder implementations; avoid fallbacks unless truly required (fallbacks add redundancy and can hide issues)
- **NEVER create documentation** unless explicitly requested, conflicts with existing docs, or absolutely critical

## Pre-Implementation Checks

- **Search first**: Before implementing any new code, search the repo by keywords to find related or similar code
- **Deduplicate**: Reuse existing logic or refactor to remove duplication when possible
- Evaluate open-source components before building from scratch
- Check for deprecated/unused code
- Assess refactoring needs and clean first if required
- Implement incrementally (one logical section at a time)

## Implementation Cycle (per section)

1. Implement a feature section
2. Write comprehensive tests for that section
3. Run tests and verify all pass
4. Fix any issues
5. Commit code

## Completion Notes

- Before ending a task, summarize current implementation status, with focus on:
  - Any implemented functionality not wired into the execution path/integrations
  - Conflicts or overlaps with existing functionality
  - Testing status and whether coverage is sufficient
  - Need for refactors/adjustments
  - Potential better implementation approaches
  - A list of next steps

## Planning & Discussion Rules

- No documentation generation during planning
- No extensive code examples (brief pseudocode only if needed)
- Keep options concise; include pros/cons, recommendation, and next steps

## Git Hooks & Restrictions

- Files: no temp scripts, test data dirs, hardcoded secrets, LICENSE text in code
- Docs: no status/progress/log docs, no PM terms (Author, Phase 1, Sprint, est.)
- Commits: `<emoji> <type>(<scope>): <subject>` (<=100 chars, <=4 lines), no PM terms
- When hooks fail: follow `<system-reminder>` and "-> Next:" guidance

## Documentation Truth Sources

`AGENTS.md` is the guardrail index, not the complete architecture source. Do not
duplicate schemas, state machines, or decision rationale outside the canonical
source. Non-canonical docs use a one-sentence summary plus a link.

- Registry publication, pinning, and replayability: `docs/adr/0035-published-versioned-registry-and-runtime-pinning.md`
- Runtime event atomicity and staging: `docs/adr/0036-runtime-commit-atomicity-and-event-buffer.md`
- Durable runtime write boundary: `docs/adr/0038-runtime-commit-boundary.md`
- Run activation identity and layering: `docs/adr/0039-run-activation-layering.md`
- Resolution and resolved run plans: `docs/adr/0040-resolver-resolved-run.md`
- Mailbox architecture: `docs/adr/0019-mailbox-architecture.md`
- Dispatch data model: `docs/adr/0022-run-dispatch-data-model.md`
- Protocol/object mapping: `docs/adr/protocol-object-model-mapping.md`

Accepted ADRs are append-mostly. Major meaning changes require a new
superseding ADR or a short amendment note that links the superseding ADR.

## Architecture Guardrails

These are hard rules derived from accepted ADRs. If a change appears to need an
exception, update the owning ADR first; do not bypass the rule in code.

- **A-G1: Durable runtime writes go through `CommitCoordinator`.** Source: [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: `ThreadCommit::validate`, runtime builder coordinator checks, and store write-entry validation. Validation: `crates/awaken-runtime/tests/builder_commit_coordinator.rs` and coordinator conformance tests in `crates/awaken-stores/tests/`.
- **A-G2: `RunRecord` persistence rejects illegal status-field combinations.** Source: [ADR-0022](docs/adr/0022-run-dispatch-data-model.md) and [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: `RunRecord::validate_for_persist` at every store write/decode boundary. Validation: run-store tests in `crates/awaken-stores/tests/`.
- **A-G3: Thread message logs are thread-owned, append-only, id-addressed, and role-valid.** Source: [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: message append validation before commit. Validation: contract and store tests under `crates/awaken-runtime-contract/` and `crates/awaken-stores/tests/`.
- **A-G4: Thread run projection cannot be overwritten by stale run updates.** Source: [ADR-0022](docs/adr/0022-run-dispatch-data-model.md). Enforcer: projection update timestamp/order validation. Validation: runtime-contract projection tests.
- **A-G5: Pinned registry resolution fails closed.** Source: [ADR-0035](docs/adr/0035-published-versioned-registry-and-runtime-pinning.md) and [ADR-0040](docs/adr/0040-resolver-resolved-run.md). Enforcer: pinned materialization errors surface as resolve/dispatch failures; live registry fallback is forbidden for pinned scope. Validation: registry graph and mailbox pinned-resolution tests.
- **A-G6: `ResolvedRunPlan::Replayable` requires snapshot provenance.** Source: [ADR-0040](docs/adr/0040-resolver-resolved-run.md). Enforcer: replayable plan construction is restricted to materialized pinned snapshots. Validation: resolution characterization tests in `crates/awaken-runtime/src/registry/resolve/pipeline/tests.rs`.
- **A-G7: Runtime event capture requires a staged commit coordinator.** Source: [ADR-0036](docs/adr/0036-runtime-commit-atomicity-and-event-buffer.md). Enforcer: mailbox construction rejects capture wiring without a staged coordinator. Validation: mailbox wiring tests in `crates/awaken-server/src/mailbox/tests.rs`.
- **A-G8: Mailbox run reads and runtime commits must be same-source.** Source: [ADR-0019](docs/adr/0019-mailbox-architecture.md) and [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: mailbox construction validates run-store/coordinator identity. Validation: mailbox construction tests in `crates/awaken-server/src/mailbox/tests.rs`.
- **A-G9: Dispatch lifecycle transitions reject impossible lease/claim combinations.** Source: [ADR-0022](docs/adr/0022-run-dispatch-data-model.md). Enforcer: `RunDispatch::validate_for_persist` at queue/store boundaries. Validation: mailbox and mailbox-store tests.
- **A-G10: Public facade exports stay narrow.** Source: [ADR-0037](docs/adr/0037-0.6.0-design-overhaul-overview.md), [ADR-0039](docs/adr/0039-run-activation-layering.md), and [ADR-0040](docs/adr/0040-resolver-resolved-run.md). Enforcer: public API compatibility tests and dependency-direction hooks. Validation: public API tests under `crates/awaken-runtime/tests/` and example compilation checks.

## Key Patterns

- Error handling: Rust `thiserror` with project error enum; use `.context()` for error chaining
- Testing: `#[cfg(test)]` modules; integration tests in `tests/`
- Linting: `cargo clippy`; `unsafe_code = "forbid"`

## Documentation Rules

- Update docs for API/schema/env var/tech debt changes only
- ADRs only when 2+ modules, new infra, or major architecture shift
- Never create docs for routine impls, refactors, optimizations, or planning

**Note**: Git hooks (`lefthook.yml`) enforce all restrictions automatically.
