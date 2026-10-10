# Contributing

This page is the shortest path from a clone to a green change. The gates are
strict on purpose; this page explains how to follow them cheaply. Every
command and path below is checked by the guards (links by `check_doc_links.py`,
commands and their flags by `check_gate_arguments.py`).

## Setup

- **Rust** is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (1.96.0);
  rustup installs it on first use. Install `cargo-nextest` too.
- **Node 22 and pnpm 11.6.0** (the version in [`package.json`](package.json)):
  `corepack prepare pnpm@11.6.0 --activate`, then
  `pnpm install --frozen-lockfile` and `npm ci --prefix e2e`.
- **Devcontainer**: [`.devcontainer/`](.devcontainer/devcontainer.json) builds
  Rust plus PostgreSQL and runs the installs above.
- **Hooks**: `lefthook install`. [`lefthook.yml`](lefthook.yml) runs the
  pre-commit, commit-msg and pre-push checks.
- **What is missing on this machine**: `scripts/ci/doctor.sh` reports each
  absent tool and the check group it costs ([`gate_tools.json`](scripts/ci/gate_tools.json)
  declares them all).

## The four entry points

| Scope | Command | Notes |
| --- | --- | --- |
| One crate | `scripts/ci/test.sh -p awaken-cli` | Adds the features and companion build the full gate supplies; `--dry-run` prints the exact commands. Extra arguments go to `cargo nextest run`. |
| Everything Rust | `scripts/ci/check-rust.sh --full` | Clippy over all features, then the workspace tests. A clippy failure skips the tests. |
| End to end | `scripts/ci/check-all.sh --group e2e` | Real binaries and processes. `scripts/ci/check-all.sh --list-groups` lists the other groups (postgres, k3d, sandbox, ...). |
| Formal | `scripts/ci/check_formal.sh --tier pr` | `--affected <base>` narrows a pr run; the default full tier runs every harness. Read [`formal/README.md`](formal/README.md) first. |

The whole release gate is `scripts/ci/check-all.sh` (hours). Before a push the
pre-push hook runs `scripts/ci/check-rust.sh --push` and
`scripts/ci/check_public_api.sh --require-tools`.

**Memory.** A full Rust build directory takes about 20 GB of disk, and on a
shared host builds may wait for memory and disk headroom. Some Kani harnesses
peak near 55 GiB of memory; when CBMC runs out of memory the formal gate reports `memory-bound`,
and the fix is a smaller harness or a bigger host, not more time.
Hand-run one harness with `scripts/ci/kani.sh`, which sets the build directory
Kani needs; `scripts/ci/bootstrap_kani.sh` installs the qualified Kani.

## Features, environment and target directories

- **Required test features** live in one place,
  [`.config/rust-tests.toml`](.config/rust-tests.toml): `workspace_features`
  (today `awaken-cli/container-k8s,awaken-sandbox/hand`) and the `[[companion]]`
  binary some tests launch (`awaken-sandbox`). `scripts/ci/test.sh` derives a
  crate's features from it and Cargo's resolve; a bare `cargo nextest run -p`
  does not.
- **`CARGO_BUILD_BUILD_DIR` must equal `CARGO_TARGET_DIR`** for tests: they find
  the companion beside their own executable. `scripts/ci/test.sh` and
  `scripts/ci/check-rust.sh --full` set it.
- **One target directory per worktree.** Without `CARGO_TARGET_DIR` the scripts
  default to `<worktree>/target` ([`_cargo_target.sh`](scripts/ci/_cargo_target.sh)).
  A target shared between worktrees reuses stale artifacts from another tree and
  produces failures that belong to neither. Builds wait silently in the host
  admission queue: an empty log is not a dead build.
- **No ambient API keys.** The gates unset provider keys
  ([`_provider_environment.sh`](scripts/ci/_provider_environment.sh)); model
  configuration goes through the config-plane API, not the environment.

## Baselines

Ratchets only shrink. Each guard owns its own flags today:

- contract shape: `python3 scripts/ci/check_contract_shape.py --tighten` records
  a fall (the hook does this with `--stage`); growth needs
  `python3 scripts/ci/check_contract_shape.py --write-baseline --reason "why"`.
- modularity: `python3 scripts/ci/_modularity_fitness.py --tighten`.
- test waits: `python3 scripts/ci/check_test_waits.py --write-baseline`.
- test inventory: `python3 scripts/ci/check_test_inventory.py --write`; a
  removed or renamed test is also recorded in
  [`test_inventory_ledger.json`](scripts/ci/test_inventory_ledger.json).
- public API snapshots: `scripts/ci/check_public_api.sh --bless`, on the
  nightly pinned as `PUBLIC_API_NIGHTLY` in
  [`check_public_api.sh`](scripts/ci/check_public_api.sh), never a rolling one.
- file limits: an oversized legacy file carries an
  `awaken-file-limit-ceiling: N` marker that may only fall.

Never edit a baseline JSON by hand; rerun the guard's own flag.

## Commits

Messages follow [`check_commit_message.py`](scripts/ci/check_commit_message.py):
`<emoji> <type>(<scope>): <subject>`, subject at most 100 characters, at most
four lines, body wrapped at 72 columns, no `Co-Authored-By` or generator
trailers, no plan markers such as slice ids. Commit with

    scripts/ci/commit.sh "🐛 fix(runtime): guard the empty commit"

which runs that validator in seconds before any hook, then calls `git commit -m`
(extra arguments are passed on). Never skip hooks. A commit that touches no
Rust may set `LEFTHOOK_EXCLUDE=rust-quick`. The file-limit hook measures staged
Rust as rustfmt will leave it, so a reflow cannot push a file over its limit.

## Known false reds

- **Missing features**: ten `awaken-cli` test binaries fail with "the `hand` role
  needs a build with `--features hand`", or a test reports the missing
  `awaken-sandbox` companion. Use `scripts/ci/test.sh` instead of bare cargo.
- **Tools not on the hook PATH**: hooks run in a non-login shell. The gates
  append `~/.local/bin` and the formal tool directory, but a `pnpm` or `node`
  installed through nvm is invisible there; link it into `~/.local/bin`.
  `scripts/ci/doctor.sh` shows what a gate cannot find.
- **Public API drift** from a different nightly: install the pinned one.
- **"0 tests run" is not a pass**, nor is a filtered run or a skipped Postgres
  test. Judge a check by its own exit status and summary line, and before
  calling a failure pre-existing, reproduce it on a clean `main` with the same
  command.
