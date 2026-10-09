# ADR-0080: Full-Gate Freshness And A Declared Tool Environment

- Status: Accepted
- Date: 2026-09-18
- Implementation: complete — `scripts/ci/gate_tools.json`, `scripts/ci/gate_tools.py`,
  `scripts/ci/check_gate_freshness.py`, `scripts/ci/reclaim_build_space.py`, and the
  stage prechecks in `check_formal.sh`, `check-rust.sh` and `pg_tests.sh`
- Amended: 2026-10-09 (§5 and Consequences: the host `awaken-worktree-gc` command
  owns build-space reclaim)

## Context

The complete gate is `scripts/ci/check-rust.sh --full`, every
`scripts/ci/check-all.sh` group, and `scripts/ci/check_formal.sh --require-tools`.
It takes hours. Because it takes hours it was, in practice, never run to
completion on `main`, and nothing in the repository recorded that fact. Four
unrelated defects accumulated behind it and each one hid the next: a solver that
was not on `PATH`, a revoked qualification receipt, a mutation configuration
inheriting a property that no longer existed, and a trace renderer keying
generations without the digest — plus three session regressions underneath them.

Two distinct defect classes produced that outcome.

**Gate staleness was invisible.** Nothing said when the full gate last passed, so
nobody could observe that the answer was "never". A gate whose last green run is
unknown is indistinguishable from a gate that has always been green, and the
difference only surfaces when a release needs it.

**Gates depended on ambient environment, and the symptom named the wrong cause.**
Every environment failure arrived disguised as a defect in this repository. An
absent solver made Kani report `0 of N failed ... VERIFICATION FAILED`, which
reads as a proof regression. An absent `kissat` made the driver report
`execvp kissat failed`. An uninstalled `e2e/node_modules` made a runner report
`ERR_MODULE_NOT_FOUND`, which reads as a broken import. An unpulled object-store
image made Docker report "pull access denied", which reads as a permissions
problem. A full disk reached CBMC as `os error 28`, truncated a GOTO binary, and
crashed the backend with `SIGABRT` minutes later. Each cost a debugging session
that started in the wrong place.

The knowledge needed to prevent all of this already existed, but scattered:
`_provider_environment.sh` knew two tool homes, `bootstrap_kani.sh` knew the
solver directory, `check_formal.sh` knew a version pin, `pg_tests.sh` knew two
container images, `check_node_dependencies.py` knew the Node roots, and
`doctor.sh` kept a prose table that nothing compared against reality.

## Decision

### 1. One record of the last green full gate

`scripts/ci/gate_record.json` names the commit the full gate last passed at, the
instant it passed, the gates that ran, and the versions of the tools it ran with.

It lives in `scripts/ci`, not `formal/`, because it covers every group — static,
docs, rust, api, contracts, formal, reliability, postgres, kubernetes, k3d,
frontend, e2e, sandbox — while `formal/` holds formal evidence only. It sits
beside the checker that reads it and the manifest whose versions it quotes.

Only `scripts/ci/check-all.sh --full-gate --record` writes it, and only after
every group passed. That entry point is one command for a scheduled run: it
selects every group, tees its output to `target/test-timings/full-gate-<instant>.log`,
prints a per-check summary, and reaches the writer only past the failure exit, so
a run with any failed check cannot record. A partial run cannot bless the record
either: the writer refuses a gate list that is not the complete group list
`check-all.sh` itself declares.

The runner captures its candidate commit before executing the checks and passes
that exact SHA to the existing writer. The writer refuses a missing or changed
candidate, modified tracked source, or new untracked source, and repeats that
fence after collecting tool versions. The previous gate record is excluded from
the source check because publishing the receipt is the final step. This binds
the record to a clean candidate at those boundaries; the worktree must remain
unchanged throughout the run. Main ancestry is still required by the separate
freshness reader and is not replaced by candidate evidence.

The quality workflow invokes this same recorded full-gate entry point and keeps
the record beside its SHA-bound logs. Its artifact includes multiple source-root
relative evidence directories. Release extracts that named artifact at checkout
root so the existing package consumer reads `target/test-timings/formal-run.json`
at the original path; it does not create another proof manifest or copy of the
test graph.

The record's integrity is checked rather than trusted. The commit it names must
exist in the repository and must be an ancestor of `main`, so a record can never
bless work that was rewritten away or that never landed.

### 2. One freshness policy, enforced mechanically

`scripts/ci/check_gate_freshness.py` compares `main` against the record. Its four
thresholds are the only copy in the repository:

| Threshold | Value |
|---|---|
| `WARN_COMMITS` | 20 commits past the record |
| `WARN_DAYS` | 3 days past the record |
| `FAIL_COMMITS` | 50 commits past the record |
| `FAIL_DAYS` | 7 days past the record |

`main` requires a fresh full gate. When a fast-lane commit lands, the record must
be refreshed within `WARN_COMMITS` commits or `WARN_DAYS` days; past
`FAIL_COMMITS` or `FAIL_DAYS` the record is expired and the branch is not
release-eligible.

The check runs in `check-all.sh`'s static group as a warning, so an ordinary
developer run is never blocked by a policy about somebody else's schedule.
`--require-fresh` is the strict mode a release or scheduled run uses: it fails on
an expired record and on a machine that has no record at all. An untrustworthy
record — a commit that is gone, a commit off `main`, a partial run — fails in
both modes, because that is a defect rather than a schedule.

### 3. One manifest of every external tool

`scripts/ci/gate_tools.json` declares every external thing the gates need: how it
is located (`PATH`, an appended bundle directory, the released Kani bundle, a
cargo subcommand, a named toolchain, a file, a container image, a Node package
root, or compiled into another tool), any version pin and who owns it, which
gates need it, and whether it is required, optional with a named skip, or fetched
on demand.

`scripts/ci/gate_tools.py` is its only reader. `doctor.sh` reports from it rather
than from a table of its own, `check_formal.sh` decides each solver through it,
and `check-rust.sh` and `pg_tests.sh` precheck through it.

### 4. A stage checks its tools and its disk before it runs

A gate stage verifies what it needs before doing anything long, so a missing tool
is always reported as a named missing tool. Solvers keep going through
`check_formal.sh`'s existing decision — present, absent and skipped by name, or
present at the wrong version and refused — now generalized to read the manifest
instead of a hard-coded case. Node roots keep going through
`check_node_dependencies.py`, which owns that decision.

Disk is checked the same way. Each long stage declares a minimum in the manifest
(formal 50G, a full Rust build 30G, postgres 10G) and refuses to start below it,
naming the stage, the filesystem, the shortfall, the reason, and the directories
that are safe to delete.

The recorded full gate also requires the named self-hosted resource contract.
`awaken-build-admission` exposes the same host policy used by Cargo; the tool
reader validates that observation without copying memory or disk thresholds.
The full entrypoint submits the complete job through the existing Cargo
external-command/deadline adapter, so its execution clock begins after host
admission and its Python, Java and container stages share that execution lane.
Nested Cargo avoids reacquiring the parent lock but rechecks actual capacity.
The source candidate is captured before admission and retained through the
logged re-entry. Ordinary developer runners retain their declared stage minima.

Check timings preserve the exact exit code and failure kind. Resource refusal
75 stops dependent work; execution timeout 124 and ordinary failures remain
distinct. All non-passing results still block the full receipt. Formal's
existing resource-bound evidence remains its exact subject/deadline-bound 124
contract; a host refusal cannot impersonate a semantic counterexample.

The shared `deadline.py` controls TERM/INT and timeout cleanup, including a
signal arriving while the child is being constructed and repeated signals
during cleanup. Parallel static workers delegate to that same control and
write the returned status. No caller owns a second cancellation policy.

### 5. Build space is reclaimed by policy, not by hand

Disk is a prerequisite like any other, and this machine ran out of it twice in
one session. 152 of 219 checkouts carried an in-tree `target/` totalling 509G
against roughly 1.1G of source each; the shared cargo intermediates had grown to
1.4T; a stale tree held another 129G. Reclaiming 1.85T by hand lost nothing,
because build output is derived.

The host's `awaken-worktree-gc` command is the sole deletion owner. It owns
candidate discovery, age thresholds, pressure reserves, process/FD references,
container binds, mounts, Git ownership, Cargo locks and final identity checks.
It removes only admitted compiler subtrees and preserves outer targets, final
binaries, gate evidence, source trees, container volumes and sccache.

As amended on 2026-10-09, `scripts/ci/reclaim_build_space.py` is a thin adapter
to that command. Default, `--summary` and `--verbose` request its dry-run report;
`--apply` delegates actual mutation to the same owner. The retired repository
7/7/3-day rules and whole-target deletion algorithm are removed. The adapter
never scans or classifies host paths and has no fallback deletion. If the host
owner is unavailable, it reports that condition explicitly. Other machines
must provide their resource owner rather than borrowing an incomplete local
filesystem policy. The command is located through the existing tool manifest.

`doctor.sh` renders the owner's observed free space and plan. A planned byte
count is not guaranteed headroom: protected or recently changed candidates may
remain, and the host admission owner must recheck actual capacity before work.

### 6. The manifest is proved against the machine

`gate_tools.py --self-test` refuses a manifest that has stopped describing
reality: every row must locate by the method it states or be legitimately absent
with the skip its gate will take; every gate name must be a real `check-all.sh`
group; every bundle directory must be one `_provider_environment.sh` actually
appends; every located hard pin must match this machine. It runs in pre-commit
and in the static group, so a row rots at the commit that rots it rather than
inside a proof hours later.

## Consequences

- The question "when did the full gate last pass?" has a checked-in answer, and
  a stale answer is visible on every static run instead of at release time.
- A record can only be written by a run that passed everything, so the record
  cannot drift into a claim nobody earned.
- A missing tool now fails before its stage rather than inside it, so the first
  error message names the cause.
- Adding an external dependency to a gate now means adding a manifest row. The
  self-test fails otherwise, which is the intended cost: undeclared tools are
  exactly how ambient environment leaked into the gates.
- The thresholds are policy and will need revisiting if the gate's runtime
  changes materially; they are in one place so that revisit is a single edit.
- Container images are declared as fetched on demand rather than required, so a
  machine with network access is not blocked by a cold cache; the engine's own
  pull failure still names the image.
- Build output is now reclaimed by a rule anyone can read and re-run, instead of
  by whoever notices the disk first. The rule is deliberately conservative: it
  keeps anything in use and anything young, so an unnecessary keep costs disk
  while an unnecessary delete would cost a rebuild at best.

## Amendment 2026-10-09: build-space consequence follows §5

The last Consequences bullet ("reclaimed by a rule anyone can read and re-run … it
keeps anything in use and anything young") describes the retired repository rule.
Since the §5 change of 2026-10-09 the repository owns no reclaim rule: the host's
`awaken-worktree-gc` command decides and deletes, and
`scripts/ci/reclaim_build_space.py` only requests its dry-run report or delegates
`--apply` to it (`scripts/ci/reclaim_build_space.py:29`,
`scripts/ci/gate_tools.json:74`). Without that command, reclaim is reported as
unavailable; there is no fallback deletion.
