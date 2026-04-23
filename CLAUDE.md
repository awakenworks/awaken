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

<!-- rtk-instructions v2 -->
# RTK (Rust Token Killer) - Token-Optimized Commands

## Golden Rule

**Always prefix commands with `rtk`**. If RTK has a dedicated filter, it uses it. If not, it passes through unchanged. This means RTK is always safe to use.

**Important**: Even in command chains with `&&`, use `rtk`:
```bash
# ❌ Wrong
git add . && git commit -m "msg" && git push

# ✅ Correct
rtk git add . && rtk git commit -m "msg" && rtk git push
```

## RTK Commands by Workflow

### Build & Compile (80-90% savings)
```bash
rtk cargo build         # Cargo build output
rtk cargo check         # Cargo check output
rtk cargo clippy        # Clippy warnings grouped by file (80%)
rtk tsc                 # TypeScript errors grouped by file/code (83%)
rtk lint                # ESLint/Biome violations grouped (84%)
rtk prettier --check    # Files needing format only (70%)
rtk next build          # Next.js build with route metrics (87%)
```

### Test (90-99% savings)
```bash
rtk cargo test          # Cargo test failures only (90%)
rtk vitest run          # Vitest failures only (99.5%)
rtk playwright test     # Playwright failures only (94%)
rtk test <cmd>          # Generic test wrapper - failures only
```

### Git (59-80% savings)
```bash
rtk git status          # Compact status
rtk git log             # Compact log (works with all git flags)
rtk git diff            # Compact diff (80%)
rtk git show            # Compact show (80%)
rtk git add             # Ultra-compact confirmations (59%)
rtk git commit          # Ultra-compact confirmations (59%)
rtk git push            # Ultra-compact confirmations
rtk git pull            # Ultra-compact confirmations
rtk git branch          # Compact branch list
rtk git fetch           # Compact fetch
rtk git stash           # Compact stash
rtk git worktree        # Compact worktree
```

Note: Git passthrough works for ALL subcommands, even those not explicitly listed.

### GitHub (26-87% savings)
```bash
rtk gh pr view <num>    # Compact PR view (87%)
rtk gh pr checks        # Compact PR checks (79%)
rtk gh run list         # Compact workflow runs (82%)
rtk gh issue list       # Compact issue list (80%)
rtk gh api              # Compact API responses (26%)
```

### JavaScript/TypeScript Tooling (70-90% savings)
```bash
rtk pnpm list           # Compact dependency tree (70%)
rtk pnpm outdated       # Compact outdated packages (80%)
rtk pnpm install        # Compact install output (90%)
rtk npm run <script>    # Compact npm script output
rtk npx <cmd>           # Compact npx command output
rtk prisma              # Prisma without ASCII art (88%)
```

### Files & Search (60-75% savings)
```bash
rtk ls <path>           # Tree format, compact (65%)
rtk read <file>         # Code reading with filtering (60%)
rtk grep <pattern>      # Search grouped by file (75%)
rtk find <pattern>      # Find grouped by directory (70%)
```

### Analysis & Debug (70-90% savings)
```bash
rtk err <cmd>           # Filter errors only from any command
rtk log <file>          # Deduplicated logs with counts
rtk json <file>         # JSON structure without values
rtk deps                # Dependency overview
rtk env                 # Environment variables compact
rtk summary <cmd>       # Smart summary of command output
rtk diff                # Ultra-compact diffs
```

### Infrastructure (85% savings)
```bash
rtk docker ps           # Compact container list
rtk docker images       # Compact image list
rtk docker logs <c>     # Deduplicated logs
rtk kubectl get         # Compact resource list
rtk kubectl logs        # Deduplicated pod logs
```

### Network (65-70% savings)
```bash
rtk curl <url>          # Compact HTTP responses (70%)
rtk wget <url>          # Compact download output (65%)
```

### Meta Commands
```bash
rtk gain                # View token savings statistics
rtk gain --history      # View command history with savings
rtk discover            # Analyze Claude Code sessions for missed RTK usage
rtk proxy <cmd>         # Run command without filtering (for debugging)
rtk init                # Add RTK instructions to CLAUDE.md
rtk init --global       # Add RTK to ~/.claude/CLAUDE.md
```

## Token Savings Overview

| Category | Commands | Typical Savings |
|----------|----------|-----------------|
| Tests | vitest, playwright, cargo test | 90-99% |
| Build | next, tsc, lint, prettier | 70-87% |
| Git | status, log, diff, add, commit | 59-80% |
| GitHub | gh pr, gh run, gh issue | 26-87% |
| Package Managers | pnpm, npm, npx | 70-90% |
| Files | ls, read, grep, find | 60-75% |
| Infrastructure | docker, kubectl | 85% |
| Network | curl, wget | 65-70% |

Overall average: **60-90% token reduction** on common development operations.
<!-- /rtk-instructions -->