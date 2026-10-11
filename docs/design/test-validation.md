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
# The same execution with checkout-local Cargo and sanitized provider settings:
bash scripts/ci/test.sh --affected --base origin/main
```

The plan includes committed changes since the base, working changes, and
untracked inputs. Global/unowned inputs fall back to the whole workspace.
The execution selects Cargo packages as well as test cases. It derives each
selected package's features from the full Cargo graph, builds each required
Hand companion once, and runs the selected packages together. Root features
alone do not reproduce features that other workspace roots enable on shared
dependencies. A host-filtered `cargo tree` comparison covers normal, build and
dev dependencies: a difference conservatively retains the canonical workspace
build and limits execution with package filters. An exact comparison permits
the narrow build. This comparison is a conservative guard, not a compiler-unit
equivalence proof. Package/feature/target overrides cannot bypass the shared
profile. Benchmark workloads remain explicit narrow experiments.
The plan retains the comparison, every command and its result; a failed
companion build stops the dependent test run. The native shell entry point uses
Git Bash on Windows.
Documentation-only Markdown changes can have no Rust owner, but still require
the documentation gate. The plan and execution receipts explicitly carry
`full_gate: false`. The source digest covers HEAD, the binary working patch,
and untracked names/bytes; a source change during execution rejects the result.
Nextest's local selection does not run doctests: changes to public examples or
compile-fail contracts also require the owner's `cargo test --doc --locked`.
Verified selections independently list Nextest's JSON inventory, then compare
binary-qualified actual successes with that inventory. Filtered and ignored
listed cases are recorded individually and reconciled with the summary;
entire filtered binaries are recorded separately without inventing their
unlisted case names. Empty
selections, renamed/missing/duplicate outcomes, unexpected exclusions, fixture
skips and failed listing/execution reject the result. Listing and execution
share one process-tree deadline; logs and source-bound JSON receipts live under
`target/test-timings/selection-*/`. A no-change affected run records `no-tests`,
never a passing execution.

Run the existing domains independently for diagnosis:

```sh
AWAKEN_STATIC_JOBS=4 bash scripts/ci/check-all.sh --group static
bash scripts/ci/check-all.sh --group rust
bash scripts/ci/pg_tests.sh --require-docker
bash scripts/ci/check-trusted-host-portability.sh
```

Required focused Rust runs list and execute identical arguments, then compare
named outcomes and occurrence counts. Equal totals with different names,
empty selections, ignored cases, fixture skips, and failed processes cannot
qualify. The listing and execution share one deadline; PostgreSQL stages also
share the existing whole-gate budget.

## Durable backend parity and remaining platform scope

`pg_tests.sh` owns one repository-parity selection, reused by the Docker gate
and an explicitly provisioned native PostgreSQL fixture:

```sh
# Caller owns fixture startup/cleanup. Missing or invalid URL fails.
AWAKEN_TEST_DATABASE_URL=... bash scripts/ci/pg_tests.sh --backend-parity
```

| Owner | Shared backend rules required in the listed selection | Additional durable evidence |
| --- | --- | --- |
| Model catalog | Memory, SQLite, PostgreSQL | Snapshot validation, corrupt rows, reopen |
| Credential | Memory, SQLite, PostgreSQL | Rotation, mutation/managed rollout recovery; fixture refusal child cases |
| DataSubject | Memory, SQLite, PostgreSQL | Exact CAS/monotone erasure, independent-connection competition and reopen on both durable backends; isolated schema cleanup |
| Captured content | Memory, SQLite, PostgreSQL | Scope/retention and exact fixture cleanup |
| WorkQueue | Memory, SQLite, PostgreSQL | Single claim, exact expiry/reclaim and stale release |
| Session/Deployment | SQLite, PostgreSQL | The complete generic port suite on both backends, plus the existing CAS/outbox rules |
| Memory resources | Volatile, SQLite, PostgreSQL | CAS/timestamps/conditional deletion and configured fixture refusal |
| Skill resources | Memory, filesystem, SQLite, PostgreSQL | Versions, pins, append and reopen |

The runner requires named concrete backend bindings before executing the whole
owned conformance target. Equal totals cannot hide a missing SQLite feature or
a renamed PostgreSQL binding. Outer cases are serial to share a finite database
budget; races inside each case remain concurrent. The separately ignored
Session capacity observation retains its existing opt-in owner. Required
libtest receipts retain names, argv, source digest, fixture admission and source
stability; real PostgreSQL must execute rather than return a successful skip.

This focused matrix does not qualify all of `pg_tests.sh --require-docker`:
physical WAL/PITR recovery, S3/MinIO lifecycle, active/active HTTP, live pool
cardinality and remaining application/store suites keep their existing full
gate owners. A native MSVC run also does not qualify Linux pidfd/FUSE/container
namespaces, real k3d, macOS Seatbelt, or paid external model APIs. Record each
lane's source SHA and actual outcomes separately. Earlier full-suite evidence
and an unchanged warm artifact cannot qualify a later commit.

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
The full-suite composition stays in the same entrypoint. Rust partitions are
available inside `check-rust.sh`, with one build, captured Cargo/binary metadata
and one full JSON inventory. Before execution, independently listed `hash:i/N`
partitions must cover every nonignored `(binary,test)` exactly once. The final
aggregator repeats that check against actual outcomes and rejects missing,
duplicate, failed or foreign-source/build/configuration shards. All workers
share one process-tree budget and split the existing total test-thread budget;
they do not each receive the full CPU count. Doctests remain a separate stage.
Ordinary Rust outcomes retain their existing fixture scope; required backend
and OS groups still supply the live-behavior qualification.

```sh
AWAKEN_RUST_SHARDS=2 NEXTEST_TEST_THREADS=8 bash scripts/ci/check-rust.sh --full
```

The default stays one partition until representative measurements establish a
benefit: Nextest already schedules tests concurrently, and repeated inventory
discovery has a cost. The quality workflow's manual `rust_shards` input enables
the comparison without weakening PR/main/release requirements.

The quality workflow retains cache-hit observations and bounded local Cargo
directory footprints. Its post-job performance observer reads the exact run
attempt's completed jobs, including the Rust cache action's post-save step,
so restore/save overhead is measured rather than inferred from compilation.
Listed cache archive sizes are separate from local directory sizes; pre-existing
self-hosted files cannot be attributed to a restore. Incomplete footprint scans
report unavailable sizes. Workspace-crate caching remains disabled.
`ci_performance.py` preserves failed/incomplete job outcomes and source/run
identities, ranks canonical stage costs and Cargo units, and reports test p50/p95
and slow cases. Performance telemetry always carries `full_gate: false`.
Missing CI data remains unavailable rather than borrowing another commit's
results. A manual `build_benchmark` input runs the representative Resource HTTP
workload at jobs 1/2 for two rounds with fresh target directories.
Both observation uploads exclude only the benchmark targets' `debug` payload.
Results, raw execution/listing logs and Cargo timing HTML stay in the artifact;
the static performance policy rejects missing exclusions or broader evidence
removal. Uploading these fresh compiler caches would add gigabytes of avoidable
artifact compression and transfer to the measurement itself.

### Ordinary build benchmarks

Use an explicit workload and compare both first builds and unchanged builds:

```sh
python3 scripts/ci/benchmark_rust_build.py \
  -p awaken-authority-clock -p awaken-lease-contract --jobs 1 2 4 --rounds 2
```

Each observation uses a fresh checkout-local Cargo target/build directory;
the order alternates across rounds. The benchmark removes temporary compiler
wrappers and output preparation, preserves ordinary profile settings, and
retains exact commands, compiler identity, source patch/digest, Cargo timing
reports, build counts, executable identities and actual passed test names.
Missing/empty executables, any skipped case, changed source or test identities,
unexpected recompilation in the unchanged warm phase, and failed commands
reject the comparison. A failed observation stops immediately and remains in
the record; there are no automatic retries. This is a selected-workload
benchmark (`full_gate: false`), not a full-workspace qualification or a general
speedup claim. Run it separately from other Cargo work.

Choose concurrency from the representative workload and current host capacity,
not just logical CPU count. In particular, Windows can run out of commit memory
even when physical memory remains available: WinError 1455 rejects the build.
Record that failure and release competing memory pressure before measuring a
heavier workload. Do not turn up global Cargo concurrency or change the system
page file as part of a benchmark. Local development keeps incremental caches;
compiler-cache experiments that disable incremental compilation belong to a
separate measured CI configuration.

On Windows, `--linker rust-lld` is an explicit alternative-linker experiment
using the active toolchain's `rust-lld.exe` with the `lld-link` flavor. It keeps
the MSVC target and records the linker condition; it does not alter the default
or qualify Microsoft's `link.exe`. Ambient Rust flags are refused for this
comparison. Preserve a failed default-linker observation independently instead
of retrying it into a successful timing sample. The native 2026-10-11 Resource
HTTP cold-build probe stopped at `ref-cast-impl` with LNK1105/error 1224 even at
jobs 1; available commit memory was about 18.6 GiB before execution. This is a
mapped-output failure, not evidence that increasing concurrency is safe.

The 2026-10-11 native MSVC probe used the two leaf packages above, 56 compiled
units and 11 executed tests per successful observation. The first round took
52.9/28.0/23.7 seconds at jobs 1/2/4; unchanged builds were under one second.
The second round reached jobs 4 in 18.7 seconds, then jobs 2 failed with
LNK1105/error 1224 while closing `serde_derive.dll`. The overall comparison is
failed, not a concurrency qualification. A separate resource HTTP jobs-2 build
also hit WinError 1455 under low commit-memory availability. Retain those
failures and establish ordinary native linker/resource stability before
changing defaults or attributing a general workspace speedup to this sample.

Resource HTTP artifact/file/repository/skill transport tests share one native
test target while retaining their original modules, cases and independent
fixtures. Memory keeps the portability gate's named target. Tests owning
one-shot process state or real child re-entry keep independent executables;
target consolidation must compare executed case identities, not just totals.

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
