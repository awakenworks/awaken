# Contributing

This page is the shortest path from a clone to a green change. The gates are
strict on purpose; this page explains how to follow them cheaply. Every
command and path below is checked by the guards (links by `check_doc_links.py`,
commands and their flags by `check_gate_arguments.py`).

## Setup

- **Rust** is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (1.96.0);
  rustup installs it on first use. Install `cargo-nextest` too.
- **Node 22 and pnpm 11.6.0** (the version in [`package.json`](package.json)):
  `corepack prepare pnpm@11.6.0 --activate`.
- **Each new worktree**: `scripts/ci/doctor.sh --fix`. It installs the
  repository root, `web` and `e2e` from their lockfiles (`pnpm install
  --frozen-lockfile --offline`, `npm ci --prefix e2e --offline`) out of the local
  package cache, with no network, and does nothing when they are already
  installed. If the cache lacks a pinned package it says so; then
  `scripts/ci/doctor.sh --fix --online` lets npm and pnpm fetch it. It never
  installs host tools or writes Git configuration.
- **Devcontainer**: [`.devcontainer/`](.devcontainer/devcontainer.json) builds
  Rust plus PostgreSQL and runs the installs above.
- **Hooks**: `lefthook install`. [`lefthook.yml`](lefthook.yml) runs the
  pre-commit, commit-msg and pre-push checks. Re-run the install after updating
  hook entry points. Pre-push always starts the checks, including when a branch
  has no upstream or pushes to a different destination; the Rust and API planners
  select the affected work. Staged-file selection still applies to pre-commit.
  The script hook also starts qualification on the first push after another
  worktree regenerated the shared hook. Missing bootstrap or qualification
  definitions fail the push. `lefthook.yml` is generated: edit
  `scripts/ci/hooks.yml` and run `python3 scripts/ci/generate_hooks.py`.
- **Cold dependency verification**: `python3 scripts/ci/check_dependency_fetch.py`
  fetches every pinned public Git revision into new object stores, then runs
  `cargo fetch --locked` from a source snapshot with an empty `CARGO_HOME`.
  It removes inherited Git/Cargo overrides and ignores global/system Git config.
  A cached unpublished commit therefore cannot make this tier green. The Linux
  and Windows quality jobs run it before restoring build caches; evidence is in
  `target/test-timings/dependency-fetch/`. The ordinary source-policy check stays
  offline. Publish a dependency commit before pinning it in this repository.
- **Git receiving repositories**: after `lefthook install`, run
  `python3 scripts/ci/install_receive_hooks.py --repository . --install` in the
  receiving repository, and verify with
  `python3 scripts/ci/install_receive_hooks.py --repository . --check`.
  The installer preserves the old hook by content hash and installs the
  standalone main guard; it is the hook's only owner (Lefthook does not manage
  `reference-transaction`), so it also guards worktrees checked out at older
  commits. Receiving runs in `GIT_DIR`, so it must not rely on finding a
  worktree's Lefthook configuration. The guard also refuses any change of main
  outside `git config awaken.mainWriter` (the integration worktree). The guard
  works for bare repositories and shared linked worktrees, preserves stdin and
  incoming Git object visibility, and rejects rewinds, merges and deletion of
  main. Re-install it after the guard changes; other hooks stay with Lefthook.
  This does not change `receive.denyCurrentBranch`, grant a history rewrite or
  replace CI. A checked-out receiving main must still satisfy Git's clean-tree
  requirements for an explicitly selected `updateInstead` receive mode.
- **What is missing on this machine**: `scripts/ci/doctor.sh` reports each
  absent tool and the check group it costs ([`gate_tools.json`](scripts/ci/gate_tools.json)
  declares them all). A gate that refuses to start names exactly what is
  missing and the one command that fixes it, e.g. `the static gate did not
  start: missing e2e/node_modules` followed by `fix: scripts/ci/doctor.sh --fix`.

For native Windows checks, use Git Bash for shell gates. In PowerShell,
select the same shell for pnpm scripts and hooks:

```powershell
$env:PNPM_CONFIG_SCRIPT_SHELL = 'C:\Program Files\Git\bin\bash.exe'
$env:PYTHONUTF8 = '1'
```

Use your Git installation's path; this setting applies to the current
PowerShell session. These settings keep quoted test patterns and `.sh`
entry points under the same shell, and read repository sources as UTF-8.

The official Managed SDK's Bash helpers require `/bin/bash`. On Windows,
install Node.js 22 or newer in a WSL Linux distribution and select it for the
SDK test gate with `$env:AWAKEN_TEST_WSL_DISTRO = 'your-distribution'`.
The gate runs all four Bash helper contracts there and fails if any of them
fails; without a selected runtime the test is reported as skipped, not passed. Other SDK tests, including filesystem confinement and private
recovery-file permissions, still execute in native Windows Node.js.

## The four entry points

| Scope | Command | Notes |
| --- | --- | --- |
| One crate | `scripts/ci/test.sh -p awaken-cli` | Compiles the authored whole test graph, filters execution to the requested package, and builds its companion once; `--dry-run` prints exact commands. Supported runner/profile arguments retain their meaning; graph-changing requests fail explicitly before tests. |
| Everything Rust | `scripts/ci/check-rust.sh --full` | Clippy over all features, then the workspace tests. A clippy failure skips the tests. |
| End to end | `scripts/ci/check-all.sh --group e2e` | Real binaries and processes. `scripts/ci/check-all.sh --list-groups` lists the other groups (postgres, k3d, sandbox, ...). |
| Formal | `scripts/ci/check_formal.sh --tier pr` | `pr` (the default) runs the critical harnesses and `--affected <base>` narrows it to changed packages; `--tier full` runs every harness and is the only receipt a release accepts. Read [`formal/README.md`](formal/README.md) first. |

The whole release gate is `scripts/ci/check-all.sh` (hours). Before a push the
pre-push hook runs `scripts/ci/check-rust.sh --push` and
`scripts/ci/check_public_api.sh --require-tools`.

**Memory.** A full Rust build directory takes about 20 GB of disk, and on a
shared host builds may wait for memory and disk headroom. Some Kani harnesses
peak near 55 GiB of memory; when CBMC runs out of memory the formal gate reports `memory-bound`,
and the fix is a smaller harness or a bigger host, not more time.
Hand-run one harness with `scripts/ci/kani.sh`, which sets the build directory
Kani needs; `scripts/ci/bootstrap_kani.sh` installs the qualified Kani.

## Affected execution and fixture ownership

`python3 scripts/ci/rust_test_plan.py --base HEAD` previews local affected tests;
add `--run` to execute the printed plan. Selection includes changed owners and
transitive consumers, including development and build dependencies. The authored
whole compilation graph and companion binaries remain unchanged.

`python3 scripts/ci/rust_test_plan.py --gate-scope --base HEAD --explain` prints
the Rust gate's scope (`none`, `packages` or `whole`), the gate scripts a change
reaches, and why each changed path counts. A CI script the gate never runs and a
document are not Rust inputs. A change to the selector itself (this planner,
`crate_test.py`, `generate_hooks.py`, `check-rust.sh` or a helper they import or
source), a manifest, the lockfile or an unowned Rust input keeps the whole run.
Neither selection nor a cache hit transfers an old receipt to a new source
revision.

CLI cross-component composition and Coordinator scenario suites live in the
existing `awaken-scenario-host` test package. CLI and Coordinator unit targets
therefore need no development edge back to that composition. Private Dream fault
injection stays in Coordinator with narrow Session/Resource fixtures, preserving
its durable crash-child and lifecycle assertions. Moving a test also moves its
fixture imports, nested modules and active evidence paths; it does not remove its
validation obligation. Compare actual per-profile compilation artifacts before
claiming a timing improvement: workspace filtering still compiles the workspace.

The Executor Testkit's `host` feature owns complete Host/Resources assembly.
Its package default retains that API; workspace Host callers enable `host`
explicitly, while Executor unit tests use only its extraction-repository and
volatile-authority factories with default features disabled. The existing
ScenarioHost target `executor_environment_composition` owns the actual
Environment/Work queue wiring and the moved Resources-authority identity case.
The Executor's private Root fixture refuses any unexpected Environment call;
it does not simulate successful Work or lease operations.

For a focused native development check, run the Testkit library both with
`--no-default-features` and with its package defaults, the Executor library with
and without `--features test-support`, and the ScenarioHost
`--test executor_environment_composition` target. Keep `--locked`, the qualified
MSVC toolchain, selected target directory and build settings stable across
comparisons. These checks supplement the affected plan; they do not replace the
canonical full gate or transfer evidence between feature profiles. A smaller
resolved package graph is structural evidence only. Cold and warm timings must
come from equivalent complete builds before reporting a build-time reduction.

## Features, environment and target directories

- **Required test features** live in one place,
  [`.config/rust-tests.toml`](.config/rust-tests.toml): `workspace_features`
  (today `awaken-cli/container-k8s,awaken-sandbox/hand`) and the `[[companion]]`
  binary some tests launch (`awaken-sandbox`). `scripts/ci/test.sh` retains the authored whole graph and filters package
  execution; copying a root's resolved features cannot preserve a peer's
  requests on shared dependencies.
- **`CARGO_BUILD_BUILD_DIR` must equal `CARGO_TARGET_DIR`** for tests: they find
  the companion beside their own executable. `scripts/ci/test.sh` and
  `scripts/ci/check-rust.sh --full` set it.
- **One target directory per worktree.** Without `CARGO_TARGET_DIR` the scripts
  default to `<worktree>/target` ([`_cargo_target.sh`](scripts/ci/_cargo_target.sh)).
  A target shared between worktrees reuses stale artifacts from another tree and
  produces failures that belong to neither. Builds wait silently in the host
  admission queue: an empty log is not a dead build.
- **Temporary files follow the target.** When `TMPDIR` is unset, gate and hook
  entry points ([`_cargo_target.sh`](scripts/ci/_cargo_target.sh)
  `awaken_configure_gate_temp`) set it to a per-run directory on the target's
  disk but outside the checkout: `<target>/gate-tmp/<pid>` for a relocated or
  symlinked target, `<checkout parent>/.<checkout>.gate-tmp/<pid>` for an
  in-tree one (a temporary crate inside the checkout would join this Cargo
  workspace). Directories of finished runs are removed at the next start. An
  explicit `TMPDIR` always wins, and one that is not a writable directory stops
  the gate by name. Native Windows shells are unchanged: their tools read
  `TEMP`/`TMP`.
- **No ambient API keys.** The gates unset provider keys
  ([`_provider_environment.sh`](scripts/ci/_provider_environment.sh)); model
  configuration goes through the config-plane API, not the environment.

## Baselines

Find a guard with `python3 scripts/ci/guards.py list`, then read its own rule
documentation with `python3 scripts/ci/guards.py explain duplicate-type`.
The navigator reads source without executing checks or writing baselines;
use `<guard>:<rule>` when two guards declare the same rule id.

Architecture naming, single-owner, contract-map, modularity and
contract shape share one CLI. Default/`--check` is read-only. `--tighten`
records decreases in an existing baseline, refuses growth even alongside a
decrease, and checks the guard's hard rules before writing. Explicit growth
requires `--write-baseline --reason "why"`; a reason never waives a hard rule.
Each guard preserves its own first-initialization rule and recorded metadata.

Use each owning script:

- contract shape: `python3 scripts/ci/check_contract_shape.py --tighten` records
  a fall (the hook does this with `--stage`); growth needs
  `python3 scripts/ci/check_contract_shape.py --write-baseline --reason "why"`.
- modularity: `python3 scripts/ci/_modularity_fitness.py --tighten` records a
  fall; raising a crate line ceiling or a long function's approved length needs
  `python3 scripts/ci/_modularity_fitness.py --write-baseline --reason '<JSON>'`
  (ADR-0091, 2026-10-31).
- architecture naming, single-owner and contract-map: their
  `_naming_fitness.py`, `_single_owner_fitness.py` and `_contract_map_fitness.py`
  scripts use the same check/tighten/reasoned-write modes.
- test waits: `python3 scripts/ci/check_test_waits.py --tighten` records decreases.
  Default/`--check` is read-only. `--write-baseline` also refuses new unexempted
  waits, even with `--reason`; convert them to a durable wait or document the
  existing timing exemption first. Short source/allow reasons are hard errors.
- test inventory: `python3 scripts/ci/check_test_inventory.py --tighten` adds
  names or raises minimum occurrence counts while retaining held requirements.
  `--write-baseline` records the current multiset after the ledger check.
  Removed or renamed tests require an authored decision in
  [`test_inventory_ledger.json`](scripts/ci/test_inventory_ledger.json);
  `--reason` never substitutes for that decision. Default/`--check` is read-only
  and retains the original empty floor when the TXT file is absent.
- public API snapshots: `scripts/ci/check_public_api.sh --bless`, on the
  nightly pinned as `PUBLIC_API_NIGHTLY` in
  [`check_public_api.sh`](scripts/ci/check_public_api.sh), never a rolling one.
- line measurements: file, function, module and crate sizes are diagnostic;
  exceeding a threshold or a recorded line count warns without blocking a
  check or commit. Structural and dependency rules remain enforced. A legacy file may carry an
  `awaken-allow(file-limit-ceiling): N` marker in the first five lines, excluding
  an opening formal header; it remains a historical measurement, not a size permission.

For a guard with a writer, use its owning flag instead of hand-editing its baseline.
The context/SPI/expiry/registry helpers have no writer: their source or metadata
decision remains the owning interface. A CLI reason cannot replace that decision.

Source exemptions use `awaken-allow(<rule>): <payload>` with the owning guard's
existing policy. `awaken-allow(secret):` applies only to its own line and permits
an empty explanation. `awaken-allow(wait): <reason>` applies on the same line or
within the three preceding lines; its timing reason must have at least 20
characters. File-limit payloads are numeric ceilings, not exemption reasons.

## Commits

Messages follow [`check_commit_message.py`](scripts/ci/check_commit_message.py):
`<emoji> <type>(<scope>): <subject>`, subject at most 100 characters, at most
four lines, body wrapped at 72 columns, no `Co-Authored-By` or generator
trailers, no plan markers such as slice ids. Commit with

    scripts/ci/commit.sh "🐛 fix(runtime): guard the empty commit"

which runs that validator in seconds before any hook, then calls `git commit -m`
(extra arguments are passed on). Never skip hooks. A commit that touches no
Rust may set `LEFTHOOK_EXCLUDE=rust-quick`. The file-limit hook measures staged
Rust as rustfmt will leave it and reports the resulting line measurements as
diagnostics. Source selection, parsing and evidence-anchor errors still fail.

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

The certificate compatibility exception is checked by
`python3 scripts/ci/check_trust_roots.py`. Only the immutable registry
`webpki-roots 0.26.11` re-export is admitted; its single real data dependency
must be registry v1.0.10 or newer. A changed checksum, source, forwarding edge,
old data version or stale exception fails the static and Rust gates. The
exception must be removed with the last compatibility consumer.
