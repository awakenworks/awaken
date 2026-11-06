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

For local Rust-only feedback, use the same coverage owner with an explicit
scope:

```sh
bash scripts/ci/combined-coverage.sh --rust-only
```

This retains the authored whole Rust feature graph, native tests, pinned
doctests, homogeneous profile export and working-source fence. It does not
start service E2E or apply its qualification threshold. `per-crate.json` records
`execution_scope=rust-workspace`; the no-argument composition below keeps
`execution_scope=combined` and every existing required stage. Scope is explicit
at the report writer, so a partial report cannot silently declare combined
execution.

Risk (P0/P1/P2) and tier (L0-L4) remain the existing dimensions above. Independent
expectation tables belong beside their tests and may be shared with proofs,
but may not call the production decision being specified. Assert both admitted
and refused effects. A default-denied optional adapter is not evidence that a
real backend executed. Required adapter lanes must provide their existing
required fixtures and reconcile named outcomes with the listed selection.

Coverage-report aggregation matches crate ownership once per source file and
reduces each measured line/branch once. Performance replay must retain the
same LCOV bytes, metadata, exclusions and identical outputs. A synthetic replay
is a report-processing benchmark only; it is neither measured product coverage
nor the paused ordinary Cargo-build benchmark. Queue, compilation, execution
and report processing retain their separate observations.

```sh
bash scripts/ci/combined-coverage.sh
```

Combined coverage runs the canonical-feature workspace tests, doctests, and
deterministic E2E on one instrumented build. Homogeneous binary-signature LCOV
groups are exported before unioning `(file,line)` and branch identities, so
repeated profiles do not multiply the denominator. `per-crate.json` reports
production `src` coverage by actual workspace membership and excludes test-only
lines. It reuses the existing Cargo module graph to exclude whole files reached
only through test modules, including their unannotated fixture helpers. Mixed
test/feature modules and files also reached by production remain candidates.
An unresolved production include makes that crate's production counters `null`
with its cause recorded; the raw LCOV evidence remains available.
`null` means unmeasured/unavailable, including missing branch data; it
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

Each recorded outcome binds its exact source, declared inputs, artifact,
toolchain, platform and execution scope. A composing product records the actual
Native, Cloud and Flow revisions and resolved artifacts in its existing BOM.
An old cohort remains evidence for that cohort; it does not certify a new one.

Provider execution, physical cleanup, native-platform behavior and SDK artifact
consumption retain separate observations. Required fixture absence is not a
successful run. A platform-specific historical packet does not select the
platform for unrelated source work.

## Integrated-source qualification record

The existing Session evidence registry holds source identity, original hashes
and scope summaries. Raw execution logs, source reconstruction patches and
bundles are stored outside the tracked source tree. Restore them into the
existing ignored `target/session-acceptance-evidence/` location when reviewing
their bytes; absence remains unavailable evidence.

Record a clean source identity and actual selected execution using the existing
recorder. Bind the actual named assertions and outcomes using the existing
acceptance owner. Preserve original bytes and their hashes when moving packets;
write a distinct sanitized summary instead of editing a hashed receipt in place.

Changing or rebasing source requires an explicit affected-input review before
inheriting a result. It never updates an old receipt to a new successful source.
The source/design checker can validate references without claiming that an
unavailable artifact or an unexecuted platform passed.
