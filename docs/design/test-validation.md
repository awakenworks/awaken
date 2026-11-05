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
`awaken.e2e.owner`/`awaken.e2e.run` and per-request volume creation labels.
The shared lifecycle refuses existing or unobservable cluster coordinates and
uses its retained creation receipts: exact Docker CIDs, the K3D creator's new
network ID, and checked volume identities. It never hands a validated name back
to K3D for a second deletion selection. Mixed or unconfirmed objects stay with
a cleanup failure; SIGKILL leftovers require explicit ownership recovery, not
an automatic next-run sweep. Private kubeconfigs stay with the original
caller-owned artifact cleanup.

The pinned K3D startup still requires its empty images volume. The same checked
volume creator selects it before K3D's idempotent ensure; the canonical archive
exporter uses the original direct stream import, so there is no second archive
store. Node effects use retained CIDs, the network is removed last by its ID,
and volume removal is non-force and never deletes a borrower to make room.
This protects normal concurrent runs following the canonical creator/cleanup
owner. Docker's volume API has no generation/UID compare-and-delete: the
per-request marker rejects existing/replaced names, but cannot defend the last
check-to-remove window against an external Docker administrator who first
violates ownership by deleting this run's volume and recreating its name.
There is no global prune/GC or additional permission layer. Product Session/Pod/
PVC UID guarantees and physical acceptance remain their own separate evidence.

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

Git pre-commit uses the same declared check inventory in `lefthook.yml`.
Its mandatory `scripts/ci/pre-commit/observe.sh` bridge creates a fresh child
build observation, then `generate_hooks.py` projects that inventory to a
temporary native `pre-commit` configuration. Lefthook retains staged-file
filtering, quoting, local configuration and `stage_fixed`; the child uses
`--no-auto-install` so its temporary view cannot replace shared Git hooks.
The temporary configuration is removed before the run is sealed. Only source
and projection digests remain in the observation. An older worktree's generic
Git hook still selects this current-worktree script on its first invocation.

Cheap hygiene/input checks run first. Remaining adjacent checks declared
`read-only` use at most four serial lanes inside a native parallel group;
small regions use a direct bounded group. Lane placement is authored in the
same inventory using frozen diagnostic costs, never selected from mutable
runtime statistics. The structure validator computes recursive concurrency
width and refuses a parallel writer or any tree that could exceed four workers.
Groups drain before a formatter, baseline writer, frontend command or Rust
gate. The outer pipeline stops after a failed group; peers in that group
finish and retain their own results. No Cargo parallelism or qualification
threshold is changed. The existing whole fallback for executor, Cargo config,
manifest, lockfile and unknown inputs remains in `rust_test_plan.py`.

The existing `parallel_check.py`/`deadline.py` worker owns deadlines and
descendant cleanup. Hook mode writes a durable initial receipt before starting
the checker, prints concise start/completion lines and retains full output in
`hooks/<id>/output.log`. Read-only workers bind the existing source/index
snapshot before and after execution; an unexpected mutation rejects the job.
Selected filenames are passed as arguments to an owned script, never evaluated
as shell code. Native temporary-config environment is restored before the
checker starts, so nested tools retain their original context.

Each `hooks/<id>/observation.json` records label, command/argument digests,
original exit code, elapsed time, collection overhead and source fence result.
The existing performance reporter emits slow hooks, first-failure latency and
unfinished counts in `hook-summary.json`/`.md`. Parallel worker durations and
nested Cargo durations are inclusive costs, not additive wall time. Corrupt or
unfinished receipts cannot become passing execution evidence; tests execute
again on every selected run. No PASS cache or second test selector is used.

For a standalone pre-commit run with the same observation lifecycle, use:

```sh
python3 scripts/ci/_native_shell.py scripts/ci/pre-commit/observe.sh --observe
```

`generate_hooks.py --check`, `_input_closure.py` and the composition guards all
read the same decoded command view. `test_hook_feedback.py` exercises real
Git/Lefthook selection, parallel barriers, automatic staging, exact filenames,
failure short-circuiting, storage faults and signal cleanup. These are local
orchestration checks; they do not qualify a full Formal or platform gate.

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
seconds under the selected build run's `rust-gate.*/`. Its `test-build` stage uses the
same canonical feature graph with `cargo test --no-run --timings`, Nextest's
underlying build command. Test discovery happens once in `test`, which then
executes that graph under the existing per-test deadline. Cargo's HTML timeline
in `target/cargo-timings/` describes the dependency path and compiler units separately from
test enumeration/runtime. Preserve both cold and warm runs before attributing
a speedup to concurrency or caching. These artifacts are diagnostic records,
not a full-gate receipt.

Every `check-all.sh`/`check-rust.sh` invocation and ordinary crate-test command
now joins one `build-observations` diagnostic bundle. The existing artifact
writer owns its identity, source capture, atomic writes and completion. Shell
re-entry inherits that context; it does not create a second run. Existing
formal and Session artifact identities remain their own evidence, never
performance qualification.

The default local root is `~/.local/state/awaken-builds`; it may be provisioned
as a symlink to the build disk. `AWAKEN_BUILD_OBSERVATIONS` overrides that root.
CI uses `RUNNER_TEMP/awaken-build-observations`, starts a context before build
work and always attempts to finish/report it. `run_id` identifies an attempt,
not a source SHA; workflow run, attempt and job are retained as external keys.
Each host Cargo call has its own `invocation_id` and optional parent. The host
launcher's committed `build_observation.py` must be installed alongside
`cargo_admission.py`; ordinary upstream Cargo binaries do not emit that host
protocol. CI platforms without it still retain task/command observations and
explicitly report missing host invocation capture.

Low-level capture adapters retain their existing source verifier and receipt.
Their command observations carry the supplied working digest where available;
they do not perform a second Git/source scan or change the verifier's read
order. A standalone sequence without a full task source snapshot explicitly
marks that capability unavailable and is excluded from automatic source
comparison. An inherited source-bound CI/gate context keeps its original owner.

The host record is written before waiting for the existing admission lock.
It separates queue time, pre-execution observation overhead and execution,
preserves refusal75, compiler errors and signals, and samples the existing host
pressure reader once per second. Samples are capped at 86,400 per invocation;
truncation and unavailable probes are explicit. Reaped-child CPU and largest
child RSS do not represent a simultaneous tree-wide memory peak. Default
sccache daemon execution can place compilers outside that process tree.
Cache snapshots describe a shared-daemon window, never an exclusive build hit
rate; resets/backend changes make the delta unavailable. No cache is reset.
SIGKILL/power loss leaves a running/queued record rather than an invented PASS.
Core storage failure prevents an unobserved start; optional metrics may be
unavailable. Observation errors do not replace an existing compiler failure.

Rust stage files live in the run directory. Registered external TSV/cache
observations are copied into it before completion, so later target rotation
cannot change the completed run. Cargo HTML is retained only when a snapshot
around the existing timed build proves that report changed; old same-SHA HTML
is never adopted. Supported observations do not require nightly Cargo JSON or
alter a caller's message format. Final linking may remain part of a compiler
unit instead of a separately measured phase.

The generated-trigger owner also checks that every declared checker is wired
into a hook. Both the early preflight and graph validation reuse that one
model, so an unwired declaration is refused before expensive suites start.

Pre-commit prints the live log path when each checker starts, then elapsed
progress every 15 seconds while it runs. Rust reports its current substage and
retains each substage's output/status; quick/push compilation retains Cargo
HTML through the same before/after collector as the full test build. The
shared-unit peer proof and conservative whole-workspace fallback stay in force.
Crate-boundary rules and Rust execution/benchmark framework cases write
individual timings into the existing task's command observations. Skips and
expected failures remain distinct from passed cases. Nested durations are
costs to inspect, never extra wall time to add to their parent hook.
The portability entrypoint owns one task and its required phases join it;
subprocess fixtures use private synthetic observations instead of inheriting
the developer's build identity.

Use the path printed by the entrypoint to inspect a completed task bundle or
a standalone Cargo invocation; the reporter emits JSON and Markdown and
archives task bundles under `target/test-timings/build-runs/<run_id>/`:

```sh
python3 scripts/ci/ci_performance.py report --path /absolute/run/evidence.json
python3 scripts/ci/ci_performance.py report --path /absolute/invocation/observation.json
python3 scripts/ci/ci_performance.py compare --root target/test-timings --output target/test-timings/comparison.json
python3 scripts/ci/ci_performance.py invocations --root "$HOME/.local/state/awaken-builds" --output target/test-timings/invocations.json
python3 scripts/ci/ci_performance.py invocations --snapshot /absolute/frozen-snapshot.json --output target/test-timings/replay.json
```

The host launcher remains the sole admission, process-lifecycle and Cargo record
discovery owner. The repository reuses that read-only protocol; task bundles,
Cargo invocations and declared synthetic observations are counted separately.
Reports retain raw recorded status beside current orphaned/unverified views,
without rewriting receipts or inventing terminal times. Workload/repository
cohorts separate feedback, Formal and qualification; old unclassified records
are not relabeled as known workloads. Exact `lane_seconds` measures lock occupancy;
legacy execution is a separate diagnostic. Inclusive parent/child execution is
never summed. Cache listener instance changes invalidate shared-daemon deltas;
unobservable manual resets and other clients still limit attribution.

On the single Linux host, `check-all.sh --group formal` defaults to cooperative
admission through the existing `_deadline.sh`/host owners. The root waits for
initial admission, then starts one unchanged total deadline (default 86,400s,
`AWAKEN_FORMAL_TIMEOUT_SECONDS`). Existing required-test phases, each Kani batch,
Java mutation/model calls and TLAPS commands re-enter the same execution lane.
The initial wait is outside that deadline; subsequent phase queues spend it.
Feedback gets priority for at most four admissions before a waiting batch gets
a turn. There is still one heavy execution at a time, with no preemption of a
running solver. `--full-gate` remains exclusive for the complete qualification.
Direct `check_formal.sh` is the gate body; use the group entry for cooperative
scheduling. An inherited exclusive gate keeps its original lease.
An old/missing host capability refuses cooperative execution with 75.

The actual Session observation mutation oracle now runs once immediately after
its real observation producer and before Kani. Its original evidence and failure
criteria are retained; TLC availability/version is checked before expensive
witness work. This catches an ineffective mutation without first spending the
long Kani run. No proof selection, Clippy coverage or release threshold is reduced.

Comparison groups retain workload, source revision, platform/host,
configuration and caller-declared `AWAKEN_BUILD_SCENARIO`. They do not infer a
cold/warm state or clean caches. Failed, source-changed and incomplete runs
are excluded from duration cohorts; sample counts are reported and build p95
is unavailable below 20 samples. The ordinary-build benchmark above remains
pending its whole-graph projection proof. Local fixture tests and exported
observations do not prove full-workspace speed or remote CI acceptance.
The paused benchmark also blocks a representative incremental A/B experiment:
no global incremental/cache policy is changed and no speedup is claimed before
the existing workload projection requirement is met. S3/OBS caching remains
deferred; shared compiler results and per-worktree mutable target/build state
keep their existing owners.

Developer experience is not qualified by a single faster run. Acceptance
targets are first visible feedback within 2 seconds, common static errors
within 10 seconds, proven non-Rust/docs warm p95 at most 30 seconds, and local
Rust warm p50 at most 60 seconds / p95 at most 120 seconds. Compare at least
20 samples of each declared scenario under matching workload/configuration;
report missing or ineligible samples as unverified. Synthetic fixtures are not
developer samples. The paused whole-graph benchmark cannot be bypassed to
claim these targets. Queue p95 targets 30 seconds, but the exclusive full gate
on this single host prevents an unconditional guarantee. The existing full
workflow is scheduled for 02:17 Asia/Singapore (18:17 UTC); push/PR/manual gate
requirements are unchanged. This schedule takes effect only after publication
to the remote default branch and availability of its registered runner.

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

### Qualification checkpoints and cohorts

The checkpoint named in ADR-0091 binds its exact candidate and stated execution
scope. The original complete Rust group qualifies V0 only when both authored
Clippy graphs, actual workspace execution and doctests pass on that source.
Report skipped cases separately. The strict full-tier formal receipt retains
its own source, derived plan and toolchain; a focused replay or historical
function body cannot qualify a later complete source. Domain reduction requires
an explicit refinement or complete equivalence-class argument for the legal
production inputs, together with the existing reachability and mutation checks.

For a composed release, retain Native commit H, Cloud commit C, Flow commit F,
resolved dependency locks and the built artifact/image identities in the existing
runtime BOM. Each composing repository must test the actual H it resolves;
passing its old cohort remains useful evidence for that old cohort. A temporary
registry or unpacked archive consumer qualifies only that artifact scenario;
registry publication and native-platform execution keep their own evidence.

V0-qualified H permits independent formal, downstream and SDK qualification
through the existing entrypoints and host queue. Declare dependencies from the
behavior each stage needs; use the host's existing resource policy rather than
an additional hourly compile allowance. Duplicate completed work is checked
before allocating another build. No stage changes another active lane's frozen
source, cancels its jobs or reclassifies a resource refusal as a pass.

The V8 record applies to the deployment architecture it ran. The user-selected
LocalWorker retirement remains after V8; it reuses the registered co-located
Worker and committed queue reads described in
[remote Task control](remote-task-control.md#queue-only-session-application-family-a-remedy-a).
Removing that callback path changes the candidate: rerun its affected matrices
and produce the final required qualification for the new source/BOM. The
Runtime SDK's independent execution boundary remains intact. Provider recovery,
physical cleanup and native Windows results stay attached to their own actual
substrates; an unavailable old environment is not a new prerequisite for every
source-only change.

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

For the v5 comparison, the original `f03c55823bbe` sample failed before export:
its exact JS response assertion omitted the SDK-required nullable `url_sources`
member. The user approved separate production-equivalent references using the
same canonically validated test-fixture/oracle corrections on both sides.
Those references keep production code, schemas, sampling tools and coverage
waivers identical to f03. Their approved non-JS exceptions are the original
writer's strict wait-baseline decreases and one `fuzz/Cargo.lock` correction:
`awaken-mcp-wire` 0.1.0 to its already-declared 0.2.0; other lock entries remain
unchanged.

The later, explicitly approved **repaired historical reference** additionally
applies only the exact three-file A2A fix normally committed and fast-forwarded
as `8e7f7d980b31`: one production Rust file, its existing regression file and
the existing JS case. It preserves ordinary first-Run streaming admission and
the existing owner-scoped refusal. This is a changed reference definition,
not an untouched-production f03 result; it does not authorize other production
fixes. Schemas, sampling tools, waivers and other approved lock differences
remain unchanged.

Record the complete path/blob/mode allowlist and actual reference commit/tree,
use fresh profiles per scope, and retain every historical failed sample without
substituting a measurement. Upstream, shutdown, source-fence or export failure
invalidates a measurement; a process exit 0 does not erase a captured panic.
An isolated terminal waiver refusal remains a failed gate even when independently valid
counters can be reviewed. Compare like-for-like served and combined counts and
ownership separately, not percentages with different changed-line denominators.

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
  and has separate original-oracle regression evidence. These `d6b21b3eee3f`
  results keep their historical subject; later normal delivery does not turn
  them into executions on a newer tree.
- **Delivered V4 (`7ca5281a4199`, tree `5e75fea042ac`).** The complete
  App/SDK/guard/clock source and its reviewed poison/fixture corrections were
  normally committed and protected-fast-forwarded. This records that source's
  delivery; the earlier `d6b21b3eee3f` suite receipts remain separate.
- **E2E continuation (`862d06298f09`, tree `5104c16e414c`).** The E2E caller and
  fixture continuation was normally committed and protected-fast-forwarded on
  V4. Its executed scope is not reassigned to later G changes.
- **SDK baseline qualifier (`b074299c9f67`, tree `745808be7fd0`).** The four-Python
  historical-baseline classification repair passed its original 49 pure tests,
  version-floor/docs checks and normal hooks, then normal commit and protected
  FF. It retains 24 current package/artifact subjects, 22 historical SemVer
  subjects and two first-release crates; code delivery is not an actual
  package/artifact/SemVer, registry or Windows release qualification.
- **G runtime qualification (tree `2903a249449aa612e2b8a3c3c73a84bef0f7d21f`).**
  The original whole-Rust entrypoint recorded 10,473 passed / 133 original
  skips, both Clippy passes and doctests. Full structure and contract-shape
  checks exited 0, the original API gate compared all 24 snapshots, and the
  original generated-contract and documentation gates passed. The original
  9,255-name retention requirement and skip accounting remain explicit;
  neither name counts nor PASS/skip totals establish a coverage baseline or
  prove branch/MC/DC, physical or product coverage.
- **G PostgreSQL follow-up.** The budget-cancelled PG attempt remains incomplete,
  with its owned cleanup results preserved. The fresh complete original run
  from the beginning passed required PG/S3/PITR in 1,208 seconds and completed
  owned cleanup with exit 0 on the runtime-qualified source; the earlier
  cancellation is not relabelled as passed.
- **G normal delivery (`a4402c05ca68c9e4313b2ca85ffff6f983219fec`, tree
  `5f032a8939f7ecbdde256d6c9577cd978567a4c7`).** Normal commit completed in
  688.095 seconds with exit 0, followed by protected FF of main and integration,
  both clean with no push. Relative to runtime-qualified tree `2903a249449a`,
  only four opening formal annotations were added in each of two files; runtime
  bodies remain byte-identical. Formal-surface classification recorded
  95 candidate / 165 selected sources and zero unclassified sources; this is
  source classification, not a proof PASS or a new verified tag.
- **Nanos-image follow-up (`fcf046d5150e1637b6ca986735fb8cd449ea1860`, tree
  `90c2adf8e7d74ad33d49b167c3f09148b78d70fe`).** The source-derived singleton
  selection of the existing full-u64 nanos-image harness passed with the
  existing Z3 profile in 1.822049229 seconds, satisfying all four covers; the
  original vacuity and formal-coverage checks and docs exited 0. Normal commit
  completed in 455.335422086 seconds with exit 0, followed by protected FF of
  clean main and integration with no push. This changes only the existing
  proof and its [trust record](formal-proof-trust.md#current-nanos-image-proof-2026-11-02);
  production time code and the other proofs remain unchanged. G's earlier
  runtime tree `2903a249449a` and normal commit `a4402c05ca68` keep their own
  results; this singleton is not a rerun of that complete runtime scope.
- **BackgroundTask consolidation (`2d195c4174b70ef079cbae52b7b085ae35c8f71e`,
  tree `87e0b06d897ae3383356bbf6f812fe6086ac2009`).** The final source-derived
  complete retained eight-harness selection passed in 27.776845485 seconds;
  after the new BackgroundTask anchor, the original nanos singleton passed
  in 1.526809494 seconds. All nine regular results and original vacuity,
  proof-input, formal-coverage, plan and docs checks exited 0. Only the old
  combined duplicate was retired; the retained scalar table uses joint ranks
  0–3 and unwind 16 with all unwinding assertions and real Drop preserved.
  Original replay, stale-fence and full-revision domains remain unchanged;
  no particular loop was identified as the root cause. The existing
  [proof-trust owner](formal-proof-trust.md#current-fixed-target-checkpoint-formulation)
  records the formulation and limits. Normal commit completed in
  468.252214448 seconds with exit 0, followed by protected FF of clean main
  and integration with no push at that checkpoint; the independent `fcf046d5150e`
  nanos history above is unchanged.
- **Historical short V6 diagnostics.** The earlier 300-second comparison
  completed succession and nanos but not independent Expiry Ord. It established
  neither a complete three-harness pass nor a failure of the original
  1,800-second ceiling. The later formulations and their exact scopes remain
  with the [proof-trust owner](formal-proof-trust.md); these short diagnostics
  are not current open-failure counts.
- **Linux Rust checkpoint `395303e5e3ab` (tree
  `a3f64c408bfe87cc3332bd05996cb19231c0a433`).** The original
  `bash scripts/ci/check-all.sh --group rust` exited 0 in 588.007369065 seconds:
  both whole-workspace Clippy graphs and doctests passed, and nextest executed
  10,511 cases successfully with 131 skipped separately. Annotated tag
  `verified/20261102-1723` binds that V0 Linux Rust scope and its original
  receipt identity; it is not a full13, Windows or release record.
- **Observation mutation repair `395303e5e3ab`.** The preceding strict full run
  on clean `3eeb9532` completed all 610 Kani harnesses, the original vacuity
  check and its TLAPS groups, then failed because an equal-clock timestamp
  mutation did not change the trace. The repair changes only the original
  mutation selector to the existing legacy clock row. The original Python
  owner passed 13 unit cases and all 21 metadata mutants; the original real
  TLC trace-mutation entry passed all base and follow-up families against
  preserved production observations. Models, schemas and producers are
  unchanged. Normal hooks and protected fast-forward delivered the repair;
  this scoped replay is not a completed fresh full-formal invocation.
- **SDK package checkpoint `395303e5e3ab`.** The original
  `check_sdk_consumer.py --qualify-package` ran its two Linux feature subjects
  successfully: all 24 crates packaged with default and all features, producing
  48 archives with the original normalized-manifest and VCS audits. This
  records packaging only; unpacked artifact consumption, compiler SemVer,
  registry publication and native Windows retain their own outcomes.
- **SDK artifact checkpoint `395303e5e3ab`.** The original artifact qualifier
  completed its 27-package archive closure, consumed 13 SDK packages from the
  verified unpacked archives, and passed all eight task cases and the sample.
  Both original major-consumer audits separately passed their 47 steps and 42
  cases. The complete SemVer qualifier did not pass: the pinned tool emitted
  unrecognized field/trait/constant rows, the approved findings predated G, and
  the existing McpWire `0.2.0` major was absent from the forced-audit cohort.
  Independent raw reports and pure parser tests do not constitute a successful
  combined qualification of its proposed replacement.
- **Native-395 downstream cohort (2026-11-03).** Flow `02b81c1c` passed the
  original complete check including required PostgreSQL, all 39 public-API
  comparisons without blessing, and the 15 critical cases without skips.
  Cloud `4e0f228a` passed its original 30 commands, including required SQL
  cases, both Phase1 history subjects and the eight selected Worker cases.
  Both retained their original ignored/skipped cases, locks, normal-hook and
  local fast-forward receipts. These are Linux source/integration results for
  their actual Native-395 compositions, not qualification of later Native
  source, production images, deployment, public release or Windows.
- **Native-a309 Rust / SDK checkpoint (2026-11-03).** Clean
  `a309bcad0fde` (tree `326f06030256`) passed the original complete
  `check-all.sh --group rust`: 10,525 nextest cases passed, 129 were skipped
  separately, and both Clippy graphs and doctests passed. Annotated
  `verified/20261103-1135` records that Linux Rust scope. The three original
  SDK package, artifact and compiler-SemVer qualifiers also exited 0 on this
  unchanged source: 24 current package subjects, 22 historical SemVer subjects
  and two first releases; the forced major audit included all 17 intended
  packages and both 45-case/50-step consumer profiles. These are neither full13
  nor Windows/public-registry qualifications.
- **Native continuation delivery.** The IAM 0.2 typed-error/atomic-publication
  integration, Org-authorized explicit principal selection, poison-guard
  consolidation and exact-identity fixture cleanup passed owning checks and
  normal hooks before protected FF. The lazy-role caller then reused the
  original binary resolver for each actual role and was normally delivered as
  `a309bcad0fde`. The later one-file `d99b2e526f35` assertion correction passed
  the original Managed capabilities entry and normal hooks before FF; it does
  not inherit a309's exact-source Rust/SDK results or repair the failed
  historical f03 sample in place.
- **Native-a309 Cloud cohort (2026-11-03).** Cloud `8e657bbb` (tree
  `8a1a97e78ac9`) resolves Native `a309bcad` and IAM `07af02a7` (0.2) through
  its original locked composition. All 32 original qualification commands
  passed, including actual PostgreSQL/directory and Worker cases, both Phase1
  history subjects and browser checks (94 passed, two original unconfigured
  deployment-OAuth cases skipped). Its normal commit and local FF passed;
  this does not qualify Flow, production images, deployment or Native coverage.
- **Native-49d Rust / SDK checkpoint (2026-11-04).** Clean
  `49d9790127bd` (tree `30271c0f05cf`) passed the original complete Linux
  Rust group: 10,525 nextest cases passed and 129 were skipped separately;
  both Clippy graphs, doctests, dependency checks and the original default-stack
  two-Run MCP regression passed. Annotated `verified/20261104-0335` records
  that Rust scope. The three original SDK qualifiers also passed on unchanged
  source: default/all each packaged 24 crates, artifact consumption checked 27
  archives plus eight tasks and the sample, and SemVer checked 22 historical
  subjects plus two first releases. Each compiler profile ran all 45 cases / 50
  steps and the 17-package forced audit; its raw nonzero major reports were
  retained and accepted only by the original strict assessor. These results
  do not qualify later source, full13, public publication or native Windows.
- **Canonical extraction-fixture delivery (2026-11-05).** Normal commit
  `dd03c06e247d` changed only the existing shared extraction fixture, stage
  recovery fixture and SQLite unit tests. It reuses the production storage
  frame and existing lossless JSON; the Pending dependency-fault case receives
  its own auxiliary Run/Thread instead of the interrupted source Run.
  The two unit cases and original prebuild/validator/crash/durable/stage process
  commands passed. After receiving the conformance-stack and deadline fixes,
  rebased `f5c7d3f91a07` retained those three blobs/modes, reran all six commands
  successfully and passed the three changed CI control scopes (45 passed,
  seven original skips) before protected FF. Whole Rust, SDK and full13 results
  are not inherited by this later source.
- **Cloud formal-tool delivery (2026-11-05).** Normal `c7b048c4` became
  protected-FF `d068b71c` after retaining the incoming main and identical five
  tool/test/document blobs. Eleven controlled launcher tests passed, including
  changed-jar rejection between Kani and TLC; the official pinned jar passed
  identity checks while its banner-probe exit 1 remained separately recorded.
  Original Kani and TLC model-command blocks stayed byte-identical. This is
  tool-selection regression evidence, not execution of the formal models or
  acceptance of a newer Native composition.
- **Native-42e Rust / SDK checkpoint (2026-11-05).** Clean
  `42e30edac45f` (tree `4a5aa1c80e1e`) passed the original complete Linux
  Rust group: 10,529 nextest cases passed, 129 original skips were reported
  separately, and both Clippy graphs, doctests, dependency checks and the
  original default-stack two-Run MCP case passed. Annotated
  `verified/20261104-1850` records that Rust scope. Fresh SDK package and artifact
  qualification passed on this source: 24 archives per profile, a 27-archive
  consumption union and eight real tasks plus the sample. Original SemVer
  records and validators completed both 45-case/50-step profiles, both
  17-package forced-major audits and both 22-package historical checks; two
  first releases remained separate. Its external collector returned 143 and
  no outer exit file survived, so the original owner-recorded exit 0 and
  byte-identical retained evidence are not relabelled as an observed outer 0.
  All three source windows closed unchanged; broader full13/live/Windows
  qualification remains separate.
- **Repaired historical reference (2026-11-05).** Normal
  `8c20deafc078` (tree `607f4b1b8940`, parent `ba8bcb492340`) adds only the
  approved A2A three-file patch to R5's eleven exceptions; all other f03
  inputs and every tracked mode were checked. Original related cases and both
  workspace Clippy graphs passed. The normal commit itself recorded exit 0;
  its external collector 143 and missing post-return record remain separately
  retained. Fresh served-process sampling passed 124 commands, then failed
  command 125's repeated Work-stop rejection assertion, leaving 89 commands
  unrun, no export and no combined measurement. Captured shutdown panic
  markers are retained independently; this is neither coverage qualification
  nor a successful original f03 sample.
- **Native-b876 lifecycle / Rust / SDK checkpoint (2026-11-05).** Normal
  `b876b2f19acf` (tree `188d19b382c9`) retains the existing prepared or mounted
  service owner through the Scenario process root, including bind failures,
  before exporter/runtime shutdown. It introduces no second lifecycle or
  constructor. Owning tests passed 557 cases with 25 original skips, six mount
  controls reached the exact existing closed-owner refusal, and the original
  default-stack 25-mode table, independent-factory model path, named Unknown
  timeout/facts and held-outbox ACK/join assertions passed. A three-blank-line
  documentation correction retained the original runtime subject through a
  canonical token-equivalence bridge; both original Clippy graphs and the fresh
  four-binary/manifest strict MCP case then passed before normal hooks and
  protected FF. The later original complete Linux Rust group passed on the clean
  committed SHA: 10,536 nextest cases passed and 129 original skips remained
  separate; both workspace Clippy graphs, doctests, dependency checks and the
  original two-Run/default-stack MCP case passed. All 70 retained owner files and
  source observations matched; annotated `verified/20261104-2106` binds this V0
  scope.
- **SDK and live evidence on b876 (2026-11-05).** A package attempt failed on
  crates.io HTTP/2 transport and remains recorded; a fresh complete package
  retry passed both profiles with 24 verified archives each. Artifact consumption
  passed 27 archives, 13 consumed packages, eight tasks without skips and the
  sample. The original SemVer qualifier completed both 45-case/50-step consumer
  profiles, 22 historical subjects, two first-release classifications and the
  strict 17-package forced-major audits; all three owner and outer-wrapper
  results were 0, with unchanged source, locks and tag. This does not repair
  the older 42e collector gap. The independent original required Managed
  Provider entry returned 1: its single Run passed, while all three exact
  child-reply attempts failed; actual coordinator/child model-target assertions
  and strict cleanup passed. Public event identity/text agree between child
  send and parent receipt, but raw provider output remains unavailable, so
  provider/adapter/Kernel attribution is unproven. No prompt, model, assertion,
  retry count or Raw/Full capture was changed to obtain a result.
- **Downstream component checkpoints.** Cloud `6fda36a9`, consuming Native
  `e9773cf5` and IAM `dc0c68b4`, passed all eight selected Worker-access cases,
  both owning Clippy checks, documentation checks, normal hooks and safe FF.
  Flow `cf4e7b83`, consuming Native `bf3e1e04`, passed 15 critical cases and its
  complete Runtime library (217 passed, three ignored separately), real
  companion, Server library check, Runtime Clippy and source-unity checks.
  The critical cases overlap that library and are not an added test total.
  Both records retain their actual cohorts; neither qualifies a newer main,
  a changed producer pin, or a deployed product.
- **Native-81 original Rust and SDK qualification (2026-11-05).** Clean
  `81aef316f004` (tree `8d361afd9051`) passed the original complete Linux Rust
  group: 10,536 passed / 129 original skips, both authored Clippy graphs,
  doctests, dependency checks and the original default-stack two-Run MCP case.
  All 70 retained owner files and source snapshots matched before annotated
  `verified/20261105-0028` was created. Fresh original SDK package, artifact and
  compiler-SemVer qualifiers and their outer wrappers then returned 0 on the
  same source: 24 archives per profile, the 27-archive consumption union,
  13 consumed packages, eight non-skipped tasks plus the sample, both
  45-case/50-step profiles, both strict 17-package forced-major assessments,
  22 historical subjects and two first-release classifications. All source
  windows closed unchanged; these results retain their Linux Rust and SDK
  scopes without claiming full13, public publication or Windows execution.
- **Native-81 real Skill journey (2026-11-05).** The original Sandbox builder
  and local Registry manifest checks passed for the immutable Sandbox image
  `sha256:f98b2ce317044a5f4ce861b16eba6225790f64b3123569deedd9930606802366`.
  The original required Skill-lazy entry then returned 1: cold and warm
  Sessions read the unknown reference and replied in 13.006s / 10.191s,
  respectively, with zero Session Sandbox Pods; preloaded-cold failed the
  original prohibition on rereading the supplied Skill body. Later restart,
  script execution and failure stages did not run. Source snapshots matched
  and the original wrapper completed its owned cleanup; image/Registry
  preparation does not establish a Pod pull or the original eight Sessions'
  physical cleanup receipts.
- **Repaired reference R7 (2026-11-05).** Normal `213190a00e82`
  (tree `5065274d4fb0`) contains the approved minimal lifecycle repair in
  exactly 22 declared f03 exception paths. Owning tests, both original Clippy
  graphs and normal hooks passed; fresh served-process sampling passed
  131 commands and failed command 132's old Environment package-network
  expectation, leaving 82 unrun. It performed no export or combined run and
  produced no coverage measurement. The sole retained panic marker belongs
  to the original corrupt-Skill negative control, whose child exit 101 and
  case success were asserted; independent all-producer termination evidence
  remains unavailable for that failed sample.
- **Independent test and diagnostic commits (2026-11-05).** Normal
  `27e7b0665192` changes one Environment JS oracle to the existing independent
  package-build/runtime-network law and the tombstone's retained Session-Work
  law. Its original Environment case and three named native controls passed,
  while the broader 214-command run passed 161 and failed ACP bridge command
  162, leaving 52 unrun; main reception remains separate. Normal
  `20be0d2e96c1` preserves the Provider's original prompts, models, assertions,
  three-attempt limit and read points, and adds only availability/equality
  observations of the already-fetched public frozen worker declaration.
  Its original controlled SDK tests increased from 68 to 78 passing cases
  without skips; unknown system/tool/MCP/Skill content is omitted, and missing
  fields never become matching empty policies. This is frozen declaration
  evidence, with real model input/output and runtime tool capabilities outside
  its scope; the historical b876 Provider failure remains recorded.
- **Downstream Native-81 frozen-candidate results (2026-11-05).** Flow's
  frozen tree `97b4b230c78e`, based on `855564b53675`, resolves Native
  `81aef316f004` and IAM `07af02a7` with its original fingerprint
  `d0a9c933cdf5e58170641d464f4113660498c0d9269fccf42c8ed18dab36fa0a`.
  Its default complete API check and whole required-PostgreSQL matrix returned
  0: all 11 required named PG cases passed without skips, source-gate tests
  passed 1,715 / 26 original skips, and browsers passed 532 / two original
  skips. All 2,318 source hashes matched and the exact created PG container
  completed create/start/die/destroy with an empty project post-state; normal
  delivery and protected reception retain separate records. Cloud's frozen
  tree `a4f9cd15e9f7`, based on `c731e8193a57` and the same producer cohort,
  retains its first full attempt's browser failure and a separate exact-case
  trace pass. Its fresh original matrix has six successful phases, including
  the complete frontend/backend check and production composition, with
  94 browser passes and two original OAuth skips; this checkpoint does not
  qualify the remaining deployment, required PG, formal or whole-matrix work.

Current open scopes and the next qualified cohort are tracked only in
[STATUS](../STATUS.md#architecture-execution). The executable full suite and
its publication record remain owned by ADR-0080 and `check-all.sh`; these
checkpoint notes do not introduce another acceptance manifest.

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
