# Test design, execution tiers, and evidence

The executable full-suite manifest is
[`check-all.sh`](../../scripts/ci/check-all.sh). This guide explains selection,
test design, and interpretation; it does not define a second suite manifest.
The native OS matrix is owned by
[`check-trusted-host-portability.sh`](../../scripts/ci/check-trusted-host-portability.sh).

## Execution tiers and risk are separate dimensions

| Tier | Purpose and substrate | Local feedback | Qualification |
| --- | --- | --- | --- |
| L0 | Format, dependency direction, ownership, schema/doc drift, checker self-tests | Changed-file hooks; bounded static checks | A check's actual outcome |
| L1 | Pure domain, codecs, contracts, properties, compile-fail doctests | Affected crate plus transitive consumers | Selected cases and feature graph |
| L2 | Real adapters, migrations, SQLite/PostgreSQL parity, concurrency, process death | A focused owned matrix with disposable fixtures | Named outcomes against the selected live backend |
| L3 | Host/CLI/Worker composition, native processes, deterministic HTTP/browser E2E | Changed composition on its native OS | Actual composed behavior and OS/substrate |
| L4 | Kani/TLA+, fuzzing, sanitizers, mutation, physical recovery and cluster/substrate gates | Relevant existing owner entrypoint | Required tool/substrate and complete owned selection |

P0 covers authority, tenant isolation, leases/fencing, secret handling, durable
commit/replay and recovery. P1 covers ordinary public API and adapter behavior.
P2 covers presentation and low-impact utilities. Assign risk to a behavior,
not an entire crate: one crate can contain all three. A P0 change requires its
decision-table, concurrency, durable failure, and composition evidence even if
the initial L1 feedback is green. A quick tier never substitutes for the
complete candidate qualification.

## Design a test before choosing its fixture

The [Session execution and resource acceptance checklist](session-execution-test-acceptance.md) calibrates the earlier 37 requirement groups against source and retained execution witnesses. Its [machine-readable catalog](session-execution-test-acceptance/catalog.json) records scenarios, assertion scopes and pending facets; it is not a second executable suite manifest or a declaration that all groups passed.

For a changed behavior, name the production entrypoint, invariant, legal state
transitions, and observable refusal/commit effect. Reuse its existing owner and
testkit rather than introducing another product contract to support a test.
Use a decision table to partition inputs; combine equivalence classes with
boundary values and the interactions capable of violating the invariant.

| Cause | Required effect/observation |
| --- | --- |
| Absent, exact, stale or mismatched identity; cross-tenant input | Exact admission/refusal and absence of unauthorized effects |
| Before/at/after expiry; owner replacement; two concurrent callers | One current authority; stale owners cannot commit; no duplicate durable effect |
| Cancellation before/after each durable boundary | Defined surviving state; bounded cleanup; no poisoned connection handed to the next caller |
| Commit succeeds but acknowledgment is lost | Restart/replay returns the same durable identity and outcome |
| Real child death at each intermediate persistence stage | Reopened production stores expose only the promised committed prefix and recoverable intent |
| Duplicate/reordered delivery or repeated cleanup | Idempotent effect, stable cursor, no secret/material leak |
| Fixture absent, invalid selector, selected test missing or skipped | Required qualification fails rather than claiming success |

Do not exhaustively multiply unrelated conditions. Use pairwise combinations
for independent ordinary settings; enumerate the complete interacting table
for P0 authority and persistence decisions. Include negative controls that
would fail if the intended invariant or composition binding were removed.
Assert durable records and externally visible effects, not just return codes.
Keep timing deterministic through owned clocks/barriers and bounded waits;
use real process death for crash durability rather than a simulated error.

The existing Config audit, Session outbox, and Credential creation/retirement
crash cases now run through the same assertions with either real SQLite or
real PostgreSQL metadata. PostgreSQL scenarios have independent schemas shared
by the parent and child. Credential encrypted material remains a separate real
SQLite persistence domain, so these tests still exercise compensation across
two durability boundaries. Selecting PostgreSQL makes connection failures
fatal. Tests use the canonical PostgreSQL checkout policy.

## Fast feedback commands

The canonical Rust feature selection lives in
[`.config/rust-tests.toml`](../../.config/rust-tests.toml). The full Rust gate and
combined coverage read it. Cargo's resolved graph, including dev dependencies,
determines affected owners and their transitive consumers:

```sh
python3 scripts/ci/rust_test_plan.py --base origin/main
python3 scripts/ci/rust_test_plan.py --base origin/main --run
bash scripts/ci/test.sh --affected --base origin/main
```

The plan includes the independent index and working-tree changes since the
base, both ends of renames, and untracked bytes. Global/unowned or manifest
inputs fall back to the whole workspace. The ordinary nextest path currently
compiles the authored whole graph and filters execution; the quick compile
path can use only the proved shared-unit peer closure. Unknown Cargo
configuration retains the original whole compile. One profile/metadata helper
constructs commands and companions, and one sequential owner retains every
command and stops after source, admission or execution failure.
Documentation-only Markdown changes can have no Rust owner, but still require
the documentation gate. The plan and execution receipts explicitly carry
`full_gate: false`. The working digest binds HEAD, index tree, staged and
working binary deltas, and untracked names/bytes; a source change during
planning or execution rejects the result. It does not qualify a staged commit.
Nextest's local selection does not run doctests: changes to public examples or
compile-fail contracts also require the owner's `cargo test --doc --locked`.

The existing ordinary-build benchmark retains its workload CLI and comparison
utilities. Execution is explicitly pending complete whole-graph test/case
projection: it starts no Cargo observations or cold targets. Its self-test is
`bash scripts/ci/test.sh --benchmark --self-test`. No cold/jobs result or
hot hook p50/p95 qualification is claimed.

Run the existing domains independently for diagnosis:

```sh
AWAKEN_STATIC_JOBS=4 bash scripts/ci/check-all.sh --group static
bash scripts/ci/check-all.sh --group rust
bash scripts/ci/pg_tests.sh --require-docker
bash scripts/ci/check-trusted-host-portability.sh
```

Harness containers must not leak Docker volumes. Postgres, MinIO, registry,
k3s and similar images declare `VOLUME`, so every `docker run`/`create` of one
names an explicit mount (`--tmpfs` for throwaway data, a run-scoped named volume
when the container restarts) and every `docker rm` passes `-v`.
`scripts/ci/check_docker_volumes.py` enforces this for `e2e/` and `scripts/` in
the static group; `scripts/ci/e2e_docker_lib.sh` labels harness objects with
`awaken.e2e.owner`/`awaken.e2e.run` and sweeps a killed run's leftovers at the
next start.

Required focused Rust runs list and execute identical arguments, then compare
named outcomes and occurrence counts. Equal totals with different names,
empty selections, ignored cases, fixture skips, and failed processes cannot
qualify. The existing host admission accepts listing and execution as one job
before their shared execution deadline starts. The ordinary command sequence
checks the same working-source identity before and after each phase; an invalid
listing starts no test run. The named receipt and reported count use the same
validated observation. Original command failures remain distinct from later
source or receipt failures. PostgreSQL stages also share the existing whole-gate
wall-clock budget; this change does not extend it or qualify a full gate.

## Reduce total time without losing evidence

Ordinary dev/test profiles use `line-tables-only`: failures retain backtrace
filenames/lines without emitting type and variable records into every native
test binary's PDB. Debug assertions, overflow checks, optimization and test
selection retain their existing defaults. For interactive variable inspection,
set `CARGO_PROFILE_DEV_DEBUG=2` and `CARGO_PROFILE_TEST_DEBUG=2`; changing profile
settings deliberately requires a fresh compatible build once.

The quality workflow retains dependency caching in the existing pinned Rust
cache action, whose keys include the toolchain, OS/architecture, compiler
environment and manifests. A warm checkout can reuse compatible workspace
artifacts locally, but that is not evidence of savings on an ephemeral CI
runner. Before enabling workspace-crate caching there, measure source freshness,
cache restore/save cost and retained artifact size alongside build time under
the actual checkout procedure. Cargo still decides freshness and all selected
tests execute on every run; a cache hit is never execution evidence.

`check-rust.sh` retains each stage's live output, exact argv, status and elapsed
seconds under `target/test-timings/rust-gate.*/`. Its `test-build` stage uses the
same canonical feature graph with `cargo test --no-run --timings`, Nextest's
underlying build command. Test discovery happens once in `test`, which then
executes that graph under the existing per-test deadline. Cargo's HTML timeline
in `target/cargo-timings/` separates the dependency path and native linking from
test enumeration/runtime. Preserve both cold and warm runs before attributing
a speedup to concurrency or caching. These artifacts are diagnostic records,
not a full-gate receipt.

1. Measure first. Preserve `target/test-timings` logs, build/test durations,
   slow cases, selected packages/features, and the source identity. Compare
   cold build, warm build, and test runtime separately under the same OS and
   resource budget. Track the longest dependency path and p50/p95 runtimes.
2. Reuse ordinary incremental builds. Reliability mutation builds use their
   own target/build cache and clean only that cache. Coverage has a separate
   instrumented cache and clears profile counters, not compiled artifacts.
   Rust-version, target-triple, feature, compiler-flag and instrumentation
   changes must not silently reuse incompatible artifacts.
3. Parallelize independent work with bounded capacity. Static Python checks
   use the same `check-all.sh` calls, fresh per-worker receipts, process-tree
   deadlines, and complete failure aggregation. Default local concurrency is
   one; CI selects four. All admitted checks drain before another group or a
   full receipt. Tune Cargo/nextest concurrency to available memory and I/O;
   more CPU jobs can make an MSVC link fail from memory pressure.
4. Isolate mutable fixtures. Use temporary roots, dynamic ports, unique schemas
   and owned clocks. A nextest resource group is appropriate only for tests
   that truly share an unavoidable substrate; fixing shared state is preferred
   to serializing an entire crate. The current runner does not introduce such
   groups without evidence that they are needed.
5. Keep the early loop narrow, then execute the necessary owned matrices.
   Run the relevant formal PR/affected tier during development, and preserve
   the existing full formal tier on main/release/nightly. Move expensive
   diagnostics out of redundant hooks, not out of required qualification.

If distributing full-suite work in the future, every shard must identify the
same candidate SHA, feature/tool/substrate configuration and expected cases.
The aggregator must reject missing/duplicate shards and partial/skipped
results. A scheduler or cache hit alone cannot produce a green full receipt.
The current implementation keeps full-suite composition unchanged.

Recorded qualification uses the canonical host admission command before work
starts. The same host policy supplies all filesystem reserves, MemAvailable and
PSI observations; repository checkers do not contain a competing percentage or
pressure policy. The existing Cargo external-command adapter admits the whole
job and starts its shared deadline after queueing. Its source SHA is retained
through the logged re-entry. Resource exit 75 stops dependent work and cannot
produce a passing record. All stages still belong to `check-all.sh`.

Real deadline tests separate process readiness from execution timing. A group
fixture first proves both signal handlers are installed, then exercises the
actual timeout/cancellation over that group. TERM/INT during spawn and repeated
signals during grace retain one cleanup and the original outcome. This does
not claim a subsecond Python startup SLO or expand production execution limits.

Do not use automatic retries to turn flaky tests green or raise the existing
300-second nextest limit to conceal hangs. Record and repair nondeterminism;
any temporary quarantine needs an owner, reason, expiry and explicit loss of
qualification. Inspect runtime and failure-rate trends before adding workers.

## Reproducible Linux and native Windows environments

The [Dev Container](../../.devcontainer/devcontainer.json) pins Rust/Node and
PostgreSQL image digests, installs locked Cargo/pnpm/npm dependencies, and uses
separate Linux Cargo and Node cache volumes. It provides the Rust/PostgreSQL
development substrate. Cluster, FUSE, namespace, formal and browser gates
still need the tools/substrates declared by their existing owners; launching
the container alone does not qualify them.

A clean environment must be able to fetch every Git SHA pinned in Cargo.lock.
During local verification the public upstreams could not serve Foundation
`2643d3f1caa114d0c651d4c867e4deed1374801c` or IAM
`deb6878bb5b19dc1c69e3c6cd5aeb46190e480ab`. The exact commits were recoverable
from the existing local Cargo cache. Seeding that cache enables testing the
same source but does not prove fresh-clone reproducibility. Publish/restore
those exact upstream objects or review a dependency-version update before
claiming a clean-machine gate succeeds; do not silently replace the pins.

Native Windows validation uses `x86_64-pc-windows-msvc`, Visual Studio C++ build
tools, and Git Bash for the existing shell entrypoint. A Linux container's
success cannot establish Windows process/path/DLL/locking behavior. Example
for a resource-constrained Windows machine, from PowerShell:

```powershell
$env:CARGO_TARGET_DIR = "$PWD/target"
$env:CARGO_BUILD_BUILD_DIR = $env:CARGO_TARGET_DIR
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_PROFILE_DEV_DEBUG = 'line-tables-only'
$env:CARGO_PROFILE_TEST_DEBUG = 'line-tables-only'
& 'C:\Program Files\Git\bin\bash.exe' scripts/ci/check-trusted-host-portability.sh
if ($LASTEXITCODE -ne 0) { throw 'Native portability validation failed' }
```

Preserve the toolchain, command, exit status, and named test results per OS.
The Rust gate uses `check_rustfmt.py` for formatting. On Windows it derives
every workspace member and target from Cargo metadata and batches the expanded
UTF-16 paths below the native command-line limit. Listing failure, an incomplete
plan and a dirty later batch all fail. Other platforms retain `cargo fmt --all`.
This checks the same workspace instead of accepting Windows error 206 or
spawning a separate Cargo metadata process for every crate.
Native Python shell probes resolve an absolute Git Bash executable (or the
explicit `AWAKEN_NATIVE_BASH`), because bare `bash` can start the WSL launcher.
They use the running Python interpreter and native file paths so the tested
entrypoints execute on Windows instead of timing out in another environment.
The probes use the shared process-tree deadline, including capture-pipe cleanup;
an ordinary Python subprocess timeout cannot retire a Windows descendant.
Ambient API-key name filtering uses case-insensitive Bash 3.2 patterns, avoiding
one external utility launch per environment variable. Its decision table
checks all 64 letter-case combinations with exact and prefixed names plus
near-miss names that must remain unchanged.

Console assets are imported from source or emitted by the brand plugins;
Vite's implicit `public` directory is disabled. Cargo must not watch an absent
`web/public`: an absent declared input invalidates every following CLI build,
including the adjacent list/run phases of a required-test selection. Verify
this boundary with two successive real Cargo invocations and the existing
Console build contract tests, rather than a synthetic cache-hit counter.

Process tests use the production retirement grace. The bridge does not shorten
its five-second cleanup observation to a test-only 100ms; timeout-only port and
copy fixtures use paused Tokio time. Windows bridge lifecycle fixtures launch
one direct Rust child, matching the native adapter's declared scope. A shell
that spawns `ping` introduces an unowned descendant and keeps captured output
open for 30 seconds; it cannot qualify direct-child retirement. Unix's private
process-group matrix separately proves real descendant retirement.

The persistent Bash tool has a different ownership scope: the whole process
tree belongs to its session. Windows attaches a kill-on-close Job Object while
the shell is suspended, before it can launch descendants. Its L2 regression
matrix observes an actual native child over an authenticated loopback
connection, with readiness before cancellation and bounded EOF/reset checks
before session reuse. Cancellation, timeout, explicit restart and tool drop
must terminate that child; cancellation also leaves an independent session's
child responsive. Successful foreground completion retains background jobs,
and the next call after invalidation has clean shell state. Existing framing,
timeout bounds, closed-stderr and numeric shell-exit tests also run on Windows;
Unix process-group and signal assertions retain their native scope.

Nextest treats a leaked process as a failed test, using the existing 100ms pipe
closure interval. The orchestration checker rejects missing, success-valued or
widened leak settings, including profile and per-test overrides. Named outcome
reconciliation also rejects `LEAK`/`LEAK-FAIL` despite a zero exit status or an
additional matching `PASS`. Assertion success with a surviving process cannot
qualify the candidate.

Durable SQLite fixtures release every repository/mount owner and await the
canonical `EmbeddedSqlite::settled()` witness before opening a replacement or
deleting their owned directory. Dropping a repository only schedules
asynchronous connection cleanup; it does not prove that Windows has released
the SQLite file handle. Copy lifecycle tests require physical fixture removal
and keep every durable head/history assertion before this closure boundary.

The portability lane qualifies every name-filtered Rust selection against
its actual execution, so a moved or empty selection cannot pass. Linux-only
Namespace replay remains owned by the required Linux lane. On native Windows,
the lane first runs the deadline's real Job tests: literal arguments/status,
Cargo dispatch, missing-Job refusal, timeout, leader exit and owner death.
The Job is attached before launching the requested command; native Cargo does
not depend on discovery of a Unix shebang/symlink addon. POSIX admission and
process-group deadlines retain their separate Linux/macOS tests.
Native crash qualification includes SQLite and live PostgreSQL separately;
Linux-specific namespace/pidfd obligations remain in the required Linux gate.

## Coverage interpretation and merge evidence

```sh
bash scripts/ci/combined-coverage.sh
```

Combined coverage runs the canonical-feature workspace tests, doctests, and
deterministic E2E on one instrumented build. Homogeneous binary-signature LCOV
groups are exported before unioning `(file,line)` and branch identities, so
repeated profiles do not multiply the denominator. `per-crate.json` reports
production `src` coverage by actual workspace membership and excludes test-only
lines. `null` means unmeasured/unavailable, including missing branch data; it
is never a reported 0% or a passing threshold. Stable line instrumentation
does not by itself prove branch/MC/DC coverage. The independent fuzz workspace
is governed by its fuzz gate, not a fabricated row in root Cargo metadata.

Prioritize uncovered P0 decision effects and transitions before improving a
global percentage. The existing changed E2E line threshold and unreachable
ratchet still run. A production-source formal anchor or inventory entry is
selection/design evidence; it does not prove that a strong production proof
or live test executed. Resolve missing strong P0 evidence in the existing
formal ledger instead of treating a complete anchor inventory as closure.

Only `bash scripts/ci/check-all.sh --full-gate --record`, with every required
group successful for the exact candidate, may write full-gate qualification.
Current focused results, partial environments, per-crate counters and quick
plans must retain their stated scope. See the existing
[full-gate policy](../adr/0080-full-gate-freshness-and-declared-tool-environment.md).

### Served-process E2E coverage

`scripts/ci/e2e-coverage.sh` measures deterministic served-process API coverage
and prints the current total and executed numerator. `stage_change_coverage_e2e.ts`
owns uniquely identified functional obligations, including the served-process
deployment mappings used to assess the G42/G43 Target guardrails; those rows do
not change guardrail status or replace the formal qualification profiles. The
post-rebase 2026-07-22 changed-line audit against `t` measured 1589/1662 =
95.61%, above the enforced strict threshold of >95%; 264 non-API-reachable
production lines are separately bounded and audited rather than hidden by broad
exclusions.

### Distributed role E2E scope

[ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md)'s
P2-B process-level acceptance ([k3d guide](../../deploy/k3d/README.md),
`e2e/k3d/distributed_control_e2e.sh`) runs the production Control process with
only a deterministic model-publication fact injected at its existing SPI,
replicated shipped Coordinator roles, two shipped `awaken-worker` processes,
signed Worker traffic, exact Credential/File/Memory/Skill realization, four
owner databases, one public API endpoint, active-active projection refresh,
Worker incarnation restart, Pod/node/Provider faults, PostgreSQL standby
promotion and concurrent load. It also hard-stops a retained Pod-bound Native
Worker, independently proves the exact old Pod exited, classifies the committed
closed `ModelOnlyV1` surface, retires only that predecessor Dispatch slot and
settles through a new Pod UID without another provider request. Worker Pods
receive no authority database or seal configuration and are checked for zero
PostgreSQL sockets.

## Integrated-source qualification record

Moved here from `STATUS.md`. Validation runs on the local Linux host; native
Windows/MSVC checks run where a Windows host exists. Source candidates, bound
runtime results, normal commits, protected fast-forwards and full13
qualification are separate claims.

- **Linux Rust / normal source (2026-11-01).** `bash scripts/ci/check-rust.sh --full`
  recorded 10,419 passed / 134 skipped, both Clippy passes and doctests;
  `check_public_api.sh`, `check_sdk_semver.py` (24-package floor), generated
  contracts and docs passed for the recorded V0 source, followed by normal
  commit and protected FF `1f3dc486ad39`. This is neither a full13 receipt nor
  validation of the later V4/G working source.
- **Required PostgreSQL follow-up.** The initial MinIO 401 was resolved with the
  same-version official binary/local repack; the subsequent full `pg_tests.sh`
  still exited 1 on four direct-execution `object_store_tests.sh` permission
  failures and the ignored Memory rename-author product gap, while PITR,
  immutable-object restoration and cleanup exited 0. The shell caller repair
  alone was normally committed and fast-forwarded as `398549b2028b`; it does
  not relabel the failed PG run as passed.
- **Memory rename-author repair (`1037e0bd8287`).** The original ignored
  regression actually failed with missing author; the sole `rename_as` path
  now forwards the caller through all three backends and the existing
  invalidating decorator. After correcting both macro-generated callers,
  `check-rust.sh --full` passed 10,420 tests / 133 original skips, both Clippy
  passes and doctests; `pg_tests.sh --require-docker`, all S3 credential phases,
  PITR, owned cleanup, API, contracts and docs passed, followed by normal
  commit and protected FF.
- **Cold recovery (`4f3737e7f71d`).** The original missing consumer failed its
  real no-source counterexample; the existing recorded-end reader is now used
  for cold Worker-process sources. The original required cases passed exact
  Exit/Superseded Repository rebuilding and byte/replay checks, no-source and
  foreign refusals, and poisoned-registry refusal; after rebase onto Memory,
  the authored two-package profile passed 778 / 16 original skips with both
  Clippy passes, doctests and docs before normal commit and protected FF.
  These are Host/Namespace receipts, not live cluster or full13 acceptance.
- **Complete V4 candidate (frozen tree `d6b21b3eee3f`).** After correcting
  actual dependency/import and fixture-clock failures, the original
  `bash scripts/ci/check-rust.sh --full` passed 10,421 tests / 133 original
  skips, both Clippy passes and doctests. The original
  `bash scripts/ci/pg_tests.sh --require-docker` passed, including S3, PITR
  restoration of the exact immutable object and owned cleanup;
  `check_public_api.sh` compared all 24 snapshots, generated contracts and
  docs passed. The follow-up poison guard reuses the canonical item reader
  and has separate original-oracle regression evidence; normal commit,
  protected FF and full13 remain separate claims.
- **G and release boundary.** G has retained-origin, inherited-permission
  and replay source changes without a compiled result; the candidate's
  `principle_status.py --guards` counts are reported in
  [STATUS](../STATUS.md#architecture-execution), not as main measurements.
  V6/L/V7/V8 and Windows remain unqualified on the final
  source; `verified/20261101-0249` (`f03c55823bbe`) and
  `verified/20261031-1847` (`af4d10ec22a1`) retain only their historical scope.

The following records are history, not current open-failure counts.

- **Session checkpoint (2026-10-30).** Accepted Session checkpoint
  `221ecbdd90ac` (accepted creation inputs and budget decisions), with Worker MCP
  updates (`d2b03a68f6`) and shared live Thread subscription admission
  (`68e55b3f0e`), was protected fast-forwarded after the formal/model update
  (`6033b7c62a`) with all production commit hooks enabled.
- **Owner/construction continuation.** Nine full package suites passed (1,939
  passed, 21 original skips), plus the original default-stack Startup case,
  full workspace Clippy and three focused Kani harnesses (30, 7 and 95 checks,
  no failures). Its callback result aliases expand to the complete original
  source bytes; pre-alias receipts keep their original subject. Fourteen full
  package suites passed 3,456 tests with 40 pre-existing skips; seven Startup
  cases include the default-stack two-Run MCP regression. Live PostgreSQL
  conformance passed 10 cases with its original capacity filter, and the store
  library passed 71. The initial Clippy refusal was repaired with a transparent
  private alias; full workspace Clippy and all 379 Application tests then passed
  on the repaired subject. The other thirteen package/Startup receipts keep their
  original source subject: mechanical alias expansion proved byte equality,
  without relabelling them as current-SHA executions.
- **Capacity candidate.** 10,008 tests across all 154 packages passed (136
  original skips) with workspace Clippy at its earlier `3268bf0eaf` base,
  including the default-stack two-Run MCP/Vault test. A later `025fa1e2b9`
  Linux ACP gate passed, and `b80a3ea2a5` workspace Clippy passed; the full b80
  run did not finish. These receipts do not establish current-main execution.
- **Recreated validation packages.** The reviewed packages of the stopped
  `validation/claude-crc-native-20261030` orchestration were recreated as normal
  commits on main after `3fb0605002`: Sandbox capacity/readiness/installation
  convergence and the test-only dependency graph, ACP execution behind features
  the Coordinator and Control products leave off, confined MemoryDir IO, the
  separate `awaken-control-process` executable with keyset discovery and IAM
  `29e073afe1`, prepared Run/Outcome admission with the phase-failure journal,
  background heartbeat checkpoints and source retention, the single
  `awaken-session-contract` and responsibility-named Session roles. They passed
  the non-compiling commit guards only. Private remote Task control and SDK
  SemVer tooling are not part of them.
- **Fourth batched verification (`9b6505d856`).** The workspace built and fmt
  and doctests passed; 39 of 10,166 tests failed, one overflowed the stack, and
  Clippy reported 7 errors. This was V0's input; the later Linux result above
  supersedes it as a failure count without extending that result to PG, cold
  recovery, or the uncommitted combined source.
- **Downstream cohort.** The earlier Cloud/Flow pin snapshot is historical. At
  the 2026-10-30 review their main refs were Cloud `31c96165dfcc` and Flow
  `b19871ca3f66`; inference-materializer imports and Flow's concrete CLI
  composition callers have migrated. Exact manifests, independent composition
  roots, locks, images and the runtime BOM must be frozen together for
  acceptance.

Still open: one final immutable BOM qualified through build, migrations, Linux
k3d/distributed execution, a real Agent-authored Environment and
coding/review/fix acceptance. Native source, unit, model and database results do
not close Cloud product acceptance.
