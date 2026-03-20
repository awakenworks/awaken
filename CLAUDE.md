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

## Key Patterns

- Error handling: Rust `thiserror` with project error enum; use `.context()` for error chaining
- Testing: `#[cfg(test)]` modules; integration tests in `tests/`
- Linting: `cargo clippy`; `unsafe_code = "forbid"`

## Documentation Rules

- Update docs for API/schema/env var/tech debt changes only
- ADRs only when 2+ modules, new infra, or major architecture shift
- Never create docs for routine impls, refactors, optimizations, or planning

**Note**: Git hooks (`lefthook.yml`) enforce all restrictions automatically.
