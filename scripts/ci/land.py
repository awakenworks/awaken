#!/usr/bin/env python3
"""Land a batch of lane branches on main with one verification of the combined tip.

    scripts/ci/land.py [--group G]... [--dry-run] [--max-rounds N] BRANCH...

Run by the integrator in the main-writer worktree (``git config awaken.mainWriter``)
on branch ``integration``. Each lane already ran its own affected checks and
commit hooks; this replays the lanes with ``git cherry-pick`` (a clean pick runs
no pre-commit hook, by design) onto current main, runs every selected gate group
once on the combined tip, and fast-forwards main with
``git fetch . integration:main`` only when that verified tip is a fast-forward
of main. Only the lanes that conflict or break a group are ejected.

Decision table (``--self-test`` exercises each row in a throwaway repository):

| Rule | situation                                              | result |
| W1   | not the writer worktree, not on ``integration``, dirty tree, a pick in progress, integration not an ancestor of main, unknown branch or group | exit 2, nothing changes |
| B1   | a lane's ``main..branch`` cherry-picks cleanly        | in the batch |
| B2   | it conflicts (or holds merge commits)                  | ejected with the conflicting paths; the batch continues from the tip before it |
| B3   | it has no commit outside main, or every pick is empty  | reported as absorbed |
| V1   | every selected group exits 0                           | land (V4) |
| V2   | a group exits 75 (host or resource refusal)            | nothing lands, nothing is ejected, exit 75: rerun later |
| V3   | a group fails otherwise                                | later groups are skipped; bisect batch prefixes with the failed group; eject the first failing prefix's last lane; rebuild and verify again |
| V5   | the bisection reaches lane 1 and main itself fails     | nothing ejected or landed, exit 1 (main or the host is broken) |
| V7   | the main-itself run of V5                              | runs the group un-narrowed: narrowed to main it selects no suite and passes vacuously |
| V4   | main moved since the batch was built                   | re-sync, rebuild on the new main, verify again |
| V6   | ``--max-rounds`` verifications without a landing       | nothing lands, exit 1 |

Each verification (V1, V3's rebuild, V4's rebuild) counts as a round; bisection
runs only the failed group and is bounded by log2 of the batch size, plus one
run on main when the culprit would be the first lane.

Group selection (without ``--group``; the table only ever adds groups, and
``--group`` replaces it entirely):

| changed path                                                    | groups |
| anything                                                        | static |
| Rust build input (``rust_test_plan.rust_inputs()``: Cargo manifests and lockfile, toolchain, ``.cargo/``, ``.config/``, ``crates/`` incl. migrations, ``fuzz/``, ``apps/``, ``tests/``, ``xtask/``, ``e2e/fixtures/``, the SDK consumer) | rust, api, contracts, formal, reliability, postgres |
| ``clippy.toml``, ``rustfmt.toml``                               | rust |
| ``docs/``, ``*.md``, ``examples/``, ``scripts/generate-example-docs.mjs`` | docs |
| ``public-api/``                                                 | api |
| ``contracts/``, ``packages/``, ``scripts/contract/``            | contracts |
| ``formal/``                                                     | formal |
| ``web/``, root ``package.json``, ``pnpm-lock.yaml``, ``pnpm-workspace.yaml`` | frontend (+ contracts for the pnpm files) |
| ``e2e/``                                                        | e2e |
| a script a group's ``run`` lines in check-all.sh name, or anything it runs or reads (``generate_hooks.script_closure``) | that group |
| ``scripts/ci/check-all.sh``: changed ``run`` lines              | their groups |
| ``scripts/ci/check-all.sh``: any other line                     | every group |

The helpers check-all.sh itself sources or runs for every group are exercised
by the static run through the same harness, so they select static only.
reliability and postgres run with ``AWAKEN_SERVICE_AFFECTED_BASE`` and formal
with ``AWAKEN_FORMAL_TIER=pr AWAKEN_FORMAL_AFFECTED_BASE``, all set to the main
commit the batch was built on, so each narrows itself to what the batch reaches.
The one exception is V7: main measured against itself reaches nothing, starts no
substrate and would pass whatever the host's state, which would eject lane 1 for
a fault main shares (an unpullable image, a dead daemon). That run leaves both
bases empty so the group runs whole. A gate that finds its substrate unavailable
exits 75 (V2) and ejects nothing either way.
kubernetes, k3d, sandbox and the e2e group are selected only by their own
runners and directories; request them with ``--group`` (listing every group).

Exit: 0 every lane landed (or a dry run); 1 a lane was ejected or nothing could
land; 2 refused before changing anything; 75 a gate refused for host
resources. The summary is printed and written as JSON beside the group logs
(``--log-dir``, default ``target/land/<UTC time>``).

For tests: ``AWAKEN_LAND_GATE`` replaces ``scripts/ci/check-all.sh`` (it is
called with ``--group G``) and ``--main-ref`` names the branch to fast-forward.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

SELF = Path(__file__).resolve()
INTEGRATION = "integration"
CHECK_ALL = "scripts/ci/check-all.sh"
GATE_ENV = "AWAKEN_LAND_GATE"
HOST_REFUSAL = 75
RUST_GROUPS = ("rust", "api", "contracts", "formal", "reliability", "postgres")
PATH_TABLE: tuple[tuple[tuple[str, ...], tuple[str, ...]], ...] = (
    (("clippy.toml", "rustfmt.toml"), ("rust",)),
    (("docs/**", "**/*.md", "examples/**", "scripts/generate-example-docs.mjs"), ("docs",)),
    (("public-api/**",), ("api",)),
    (("contracts/**", "packages/**", "scripts/contract/**"), ("contracts",)),
    (("formal/**",), ("formal",)),
    (("web/**",), ("frontend",)),
    (("package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"), ("frontend", "contracts")),
    (("e2e/**",), ("e2e",)),
)
RUN_LINE = re.compile(r'^run ([a-z0-9-]+) "[^"]*" (.*)$')
GROUPS_LINE = re.compile(r"^groups=\(([^)]*)\)$", re.M)
PATH_TOKEN = re.compile(r"(?<![\w/.-])((?:scripts|e2e|deploy)/[A-Za-z0-9_./-]+\.(?:sh|py|mjs))")


class Refused(Exception):
    """A precondition failed before anything changed (exit 2)."""


class HostRefused(Exception):
    """A gate exited 75: the host could not run it now."""


def environment(extra: dict[str, str] | None = None) -> dict[str, str]:
    """The caller's environment without Git's repository overrides.

    A hook exports GIT_DIR and GIT_INDEX_FILE for the commit in progress; a git
    call (or a gate) that inherits them acts on that repository instead.
    """
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(extra or {})
    return env


class Git:
    def __init__(self, top: Path):
        self.top = top

    def run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        result = subprocess.run(["git", *args], cwd=self.top, env=environment(), capture_output=True,
                                text=True, stdin=subprocess.DEVNULL)
        if check and result.returncode:
            raise RuntimeError(f"git {' '.join(args)} exited {result.returncode}: {result.stderr.strip()}")
        return result

    def out(self, *args: str) -> str:
        return self.run(*args).stdout.strip()

    def commit(self, ref: str) -> str | None:
        result = self.run("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}", check=False)
        return result.stdout.strip() if result.returncode == 0 else None

    def ancestor(self, old: str, new: str) -> bool:
        return self.run("merge-base", "--is-ancestor", old, new, check=False).returncode == 0

    def picking(self) -> bool:
        path = self.out("rev-parse", "--path-format=absolute", "--git-path", "CHERRY_PICK_HEAD")
        return Path(path).exists()

    def reset(self, commit: str) -> None:
        if self.picking():
            self.run("cherry-pick", "--abort", check=False)
        self.run("reset", "--quiet", "--hard", commit)

    def show(self, commit: str, path: str) -> str | None:
        result = self.run("show", f"{commit}:{path}", check=False)
        return result.stdout if result.returncode == 0 else None


@dataclass
class Lane:
    branch: str
    start: str
    end: str
    commits: int


@dataclass
class Batch:
    base: str
    lanes: list[Lane] = field(default_factory=list)

    @property
    def tip(self) -> str:
        return self.lanes[-1].end if self.lanes else self.base


@dataclass
class Context:
    git: Git
    main_ref: str
    gate: list[str]
    log_dir: Path
    known_groups: list[str]
    requested: list[str]
    runs: list[dict] = field(default_factory=list)
    ejected: list[dict] = field(default_factory=list)
    absorbed: list[str] = field(default_factory=list)


# --- batch -------------------------------------------------------------------------------------------

def build(ctx: Context, base: str, branches: list[str]) -> Batch:
    """Replay each lane's ``base..branch`` on ``base`` in order; eject what does not apply (B1-B3)."""
    git, batch = ctx.git, Batch(base)
    git.reset(base)
    for branch in branches:
        commits = git.out("rev-list", "--reverse", f"{base}..{branch}").split()
        if not commits:
            ctx.absorbed.append(branch)
            continue
        if git.out("rev-list", "--merges", f"{base}..{branch}"):
            eject(ctx, branch, "merge commits (rebase the lane linearly)")
            continue
        start = git.out("rev-parse", "HEAD")
        result = git.run("cherry-pick", f"{base}..{branch}", check=False)
        while result.returncode:
            conflicts = git.out("diff", "--name-only", "--diff-filter=U").split()
            clean = not git.out("status", "--porcelain=v1", "--untracked-files=no")
            if not conflicts and clean and git.picking():
                result = git.run("cherry-pick", "--skip", check=False)  # already in main: drop it
                continue
            git.reset(start)
            if conflicts:
                eject(ctx, branch, "conflict in " + ", ".join(conflicts), paths=conflicts)
            else:
                eject(ctx, branch, "cherry-pick failed: " + (result.stderr.strip().splitlines() or ["?"])[-1])
            break
        else:
            end = git.out("rev-parse", "HEAD")
            if end == start:
                ctx.absorbed.append(branch)
            else:
                count = int(git.out("rev-list", "--count", f"{start}..{end}"))
                batch.lanes.append(Lane(branch, start, end, count))
    return batch


def eject(ctx: Context, branch: str, reason: str, *, log: str | None = None, paths: list[str] | None = None):
    entry = {"branch": branch, "reason": reason, "log": log, "paths": paths or []}
    ctx.ejected.append(entry)
    print(f"✗ eject {branch}: {reason}" + (f" (log {log})" if log else ""), flush=True)


# --- group selection ---------------------------------------------------------------------------------

def run_lines(text: str) -> tuple[dict[str, list[str]], list[str]]:
    """check-all.sh's ``run <group>`` lines by group, and every other code line."""
    groups: dict[str, list[str]] = {}
    other = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = RUN_LINE.match(line)
        if match:
            groups.setdefault(match.group(1), []).append(line)
        else:
            other.append(stripped)
    return groups, other


def group_owners(text: str) -> dict[str, set[str]]:
    """``{path: groups}`` for every script a non-static group runs, and what that script runs or reads."""
    import generate_hooks
    owners: dict[str, set[str]] = {}
    for group, lines in run_lines(text)[0].items():
        if group == "static":
            continue
        for line in lines:
            for token in PATH_TOKEN.findall(line):
                owners.setdefault(token, set()).add(group)
                if token.startswith("scripts/ci/"):
                    for name in generate_hooks.script_closure(token.removeprefix("scripts/ci/")):
                        owners.setdefault(f"scripts/ci/{name}", set()).add(group)
    return owners


def check_all_change(base_text: str | None, tip_text: str, known: list[str]) -> tuple[list[str], str]:
    """Groups a change of check-all.sh can alter, and why."""
    if base_text is None:
        return known, "new at the batch base"
    before, after = run_lines(base_text), run_lines(tip_text)
    if before[1] != after[1]:
        return known, "a line outside the run lines changed"
    changed = sorted(group for group in {*before[0], *after[0]}
                     if before[0].get(group) != after[0].get(group))
    return changed, "its run lines changed"


def select_groups(changed: list[str], tip_text: str | None, base_text: str | None,
                  known: list[str]) -> list[tuple[str, str]]:
    """``[(group, why)]`` in check-all.sh order; ``why`` names the path that selected the group."""
    import generate_hooks
    import rust_test_plan
    reasons = {"static": "always"}

    def add(groups, why: str) -> None:
        for group in groups:
            reasons.setdefault(group, why)

    rust = rust_test_plan.rust_inputs()
    owners = group_owners(tip_text) if tip_text else {}
    for path in changed:
        glob = next((glob for glob in rust if generate_hooks.glob_matches(glob, path)), None)
        if glob:
            add(RUST_GROUPS, f"{path} (Rust build input {glob})")
        for globs, groups in PATH_TABLE:
            glob = next((glob for glob in globs if generate_hooks.glob_matches(glob, path)), None)
            if glob:
                add(groups, f"{path} (matches {glob})")
        if path != CHECK_ALL:  # check-all.sh is judged by its own rule below
            for group in sorted(owners.get(path, ())):
                add([group], f"{path} (run or read by the {group} group)")
        elif tip_text is not None:
            groups, why = check_all_change(base_text, tip_text, known)
            add(groups, f"{path} ({why})")
    order = {group: index for index, group in enumerate(known)}
    return sorted(reasons.items(), key=lambda item: order.get(item[0], len(order)))


def plan_groups(ctx: Context, batch: Batch) -> list[tuple[str, str]]:
    if ctx.requested:
        return [(group, "requested with --group") for group in ctx.requested]
    changed = ctx.git.out("diff", "--name-only", batch.base, batch.tip).splitlines()
    tip_text = ctx.git.show(batch.tip, CHECK_ALL)
    return select_groups(changed, tip_text, ctx.git.show(batch.base, CHECK_ALL), ctx.known_groups)


# --- verification ------------------------------------------------------------------------------------

def run_group(ctx: Context, group: str, tip: str, base: str, label: str) -> dict:
    """Run one group on ``tip`` (checked out) with the affected bases set to ``base``.

    ``tip == base`` is main measured against itself (V7): narrowed, it would
    select nothing and pass vacuously, so both bases are left empty and the
    group runs whole.
    """
    ctx.git.reset(tip)
    log = ctx.log_dir / f"{label}-{group}.log"
    affected = "" if tip == base else base
    env = environment({"AWAKEN_SERVICE_AFFECTED_BASE": affected, "AWAKEN_FORMAL_TIER": "pr",
                       "AWAKEN_FORMAL_AFFECTED_BASE": affected})
    scope = "" if affected else " un-narrowed"
    print(f"→ {label} {group}{scope} on {tip[:10]} (log {log})", flush=True)
    started = time.monotonic()
    with log.open("w", encoding="utf-8") as out:
        try:
            code = subprocess.run([*ctx.gate, "--group", group], cwd=ctx.git.top, env=env, stdout=out,
                                  stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL).returncode
        except OSError as error:
            out.write(f"land.py: could not start the gate: {error}\n")
            code = 127
    seconds = round(time.monotonic() - started, 1)
    verdict = "passed" if code == 0 else "host refusal" if code == HOST_REFUSAL else "failed"
    print(f"  {group}: {verdict} (exit {code}) in {seconds}s", flush=True)
    record = {"label": label, "group": group, "tip": tip, "base": base, "exit": code,
              "seconds": seconds, "log": str(log)}
    ctx.runs.append(record)
    return record


def verify(ctx: Context, groups: list[str], tip: str, base: str, label: str) -> dict | None:
    """Run ``groups`` in order; the first failing run (V3), None when all pass. Raises on 75 (V2)."""
    for group in groups:
        record = run_group(ctx, group, tip, base, label)
        if record["exit"] == HOST_REFUSAL:
            raise HostRefused(record)
        if record["exit"]:
            return record
    return None


def bisect(ctx: Context, batch: Batch, failure: dict, label: str) -> tuple[Lane, dict] | None:
    """The lane whose prefix first fails the failed group, or None when main itself fails (V5)."""
    groups = [failure["group"]]
    failures = {len(batch.lanes): failure}
    print(f"bisecting {failure['group']} over {len(batch.lanes)} lane(s)", flush=True)

    def fails(size: int) -> bool:
        tip = batch.lanes[size - 1].end if size else batch.base
        record = verify(ctx, groups, tip, batch.base, f"{label}-prefix{size}")
        if record:
            failures[size] = record
        return record is not None

    passing, failing = 0, len(batch.lanes)
    while failing - passing > 1:
        middle = (passing + failing) // 2
        if fails(middle):
            failing = middle
        else:
            passing = middle
    if failing == 1 and fails(0):
        return None
    return batch.lanes[failing - 1], failures[failing]


def sync(ctx: Context, reset_to: str | None) -> str:
    """Fast-forward integration to main and return main's commit."""
    git = ctx.git
    if reset_to:
        git.reset(reset_to)
    main = git.commit(ctx.main_ref)
    if not main:
        raise Refused(f"{ctx.main_ref} does not name a commit")
    if not git.ancestor("HEAD", main):
        raise Refused(f"{INTEGRATION} is not an ancestor of {ctx.main_ref}; land or drop its own commits first")
    git.run("merge", "--quiet", "--ff-only", main)
    return main


def land(ctx: Context, branches: list[str], dry_run: bool, max_rounds: int) -> tuple[str, Batch | None, list]:
    """The rounds of build → verify → (bisect, eject) → land. Returns (status, landed batch, groups)."""
    git, lanes, rounds, base, groups = ctx.git, list(branches), 0, None, []
    main_ref = f"refs/heads/{ctx.main_ref}"
    while True:
        base = sync(ctx, base)
        batch = build(ctx, base, lanes)
        lanes = [lane.branch for lane in batch.lanes]
        if not lanes:
            print("nothing to land: every lane was ejected or is already in main", flush=True)
            return "nothing-to-land", batch, groups
        groups = plan_groups(ctx, batch)
        print(f"batch on {ctx.main_ref} {base[:10]}:", flush=True)
        for lane in batch.lanes:
            print(f"  {lane.branch}: {lane.commits} commit(s) → {lane.start[:10]}..{lane.end[:10]}")
        print("groups:")
        for group, why in groups:
            print(f"  {group}: {why}")
        if dry_run:
            print("dry run: no gate ran and main was not moved", flush=True)
            return "dry-run", batch, groups
        if rounds >= max_rounds:
            print(f"✗ {max_rounds} verification round(s) used without landing; nothing landed", flush=True)
            return "round-limit", None, groups
        rounds += 1
        label = f"round{rounds}"
        failure = verify(ctx, [group for group, _ in groups], batch.tip, base, label)
        if failure:
            found = bisect(ctx, batch, failure, label)
            if found is None:
                print(f"✗ {failure['group']} fails on {ctx.main_ref} {base[:10]} itself; no lane is ejected "
                      f"and nothing landed (log {ctx.runs[-1]['log']})", flush=True)
                return "main-fails", None, groups
            culprit, record = found
            eject(ctx, culprit.branch, f"group {record['group']} failed", log=record["log"])
            lanes.remove(culprit.branch)
            continue
        main_now = git.commit(ctx.main_ref)
        if main_now != base or not git.ancestor(main_now, batch.tip):
            print(f"{ctx.main_ref} moved to {main_now[:10]} during verification; rebuilding on it", flush=True)
            continue
        git.reset(batch.tip)
        pushed = git.run("fetch", "--quiet", ".", f"refs/heads/{INTEGRATION}:{main_ref}", check=False)
        if pushed.returncode:
            if git.commit(ctx.main_ref) != base:
                print(f"{ctx.main_ref} moved while landing; rebuilding on it", flush=True)
                continue
            raise RuntimeError(f"fast-forward of {ctx.main_ref} failed: {pushed.stderr.strip()}")
        return "landed", batch, groups


# --- entry -------------------------------------------------------------------------------------------

def preconditions(git: Git, main_ref: str, branches: list[str], requested: list[str], known: list[str]) -> None:
    """W1: refuse anything but a clean writer worktree on integration with resolvable inputs."""
    writer = git.run("config", "--get", "awaken.mainWriter", check=False).stdout.strip()
    if not writer or os.path.realpath(writer) != os.path.realpath(git.top):
        raise Refused(f"run this in the main-writer worktree (git config awaken.mainWriter = {writer or 'unset'}), "
                      f"not {git.top}")
    branch = git.run("symbolic-ref", "--quiet", "--short", "HEAD", check=False).stdout.strip()
    if branch != INTEGRATION:
        raise Refused(f"the writer worktree must be on branch {INTEGRATION}, not {branch or 'a detached HEAD'}")
    if git.picking() or git.out("status", "--porcelain=v1", "--untracked-files=all"):
        raise Refused("the writer worktree must be clean (no changes, untracked files or cherry-pick in progress)")
    if not git.commit(main_ref):
        raise Refused(f"{main_ref} does not name a commit")
    if not git.ancestor("HEAD", main_ref):
        raise Refused(f"{INTEGRATION} is not an ancestor of {main_ref}; land or drop its own commits first")
    if not branches:
        raise Refused("name at least one lane branch")
    if len(set(branches)) != len(branches):
        raise Refused("a lane branch is named twice")
    for name in branches:
        if not git.commit(name):
            raise Refused(f"lane {name} does not name a commit")
    unknown = [group for group in requested if known and group not in known]
    if unknown:
        raise Refused(f"unknown group(s) {', '.join(unknown)}; check-all.sh has {', '.join(known)}")


def summarize(ctx: Context, status: str, batch: Batch | None, groups: list, path: Path) -> dict:
    landed = []
    if status == "landed" and batch:
        landed = [{"branch": lane.branch, "range": f"{lane.start}..{lane.end}", "commits": lane.commits}
                  for lane in batch.lanes]
    summary = {"status": status, "main_ref": ctx.main_ref, "landed_tip": batch.tip if landed else None,
               "landed": landed, "ejected": ctx.ejected, "absorbed": ctx.absorbed,
               "groups": [{"group": group, "why": why} for group, why in groups], "runs": ctx.runs}
    print(f"\nland summary: {status}")
    for lane in landed:
        start, end = lane["range"].split("..")
        print(f"  landed  {lane['branch']} → {start[:10]}..{end[:10]} ({lane['commits']} commit(s))")
    for entry in ctx.ejected:
        print(f"  ejected {entry['branch']}: {entry['reason']}" + (f" (log {entry['log']})" if entry["log"] else ""))
    for branch in summary["absorbed"]:
        print(f"  absorbed {branch}: no commit outside {ctx.main_ref}")
    for run in ctx.runs:
        print(f"  ran     {run['label']} {run['group']}: exit {run['exit']} in {run['seconds']}s ({run['log']})")
    path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(f"  summary {path}", flush=True)
    return summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("branches", nargs="*", metavar="BRANCH", help="lane branches, landed in this order")
    parser.add_argument("--group", action="append", default=[], help="run exactly these groups (repeatable)")
    parser.add_argument("--dry-run", action="store_true", help="build the batch and print the plan only")
    parser.add_argument("--max-rounds", type=int, default=3, help="verifications before giving up (default 3)")
    parser.add_argument("--log-dir", type=Path, help="group logs and summary.json (default target/land/<time>)")
    parser.add_argument("--main-ref", default="main", help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true", help="run the decision table in a scratch repository")
    args = parser.parse_args(argv)
    if args.self_test:
        self_test()
        return 0
    if args.max_rounds < 1:
        parser.error("--max-rounds must be at least 1")
    found = subprocess.run(["git", "rev-parse", "--show-toplevel"], env=environment(), capture_output=True, text=True)
    if found.returncode:
        print("land: not inside a git worktree", file=sys.stderr)
        return 2
    git = Git(Path(found.stdout.strip()))
    check_all = git.top / CHECK_ALL
    listed = GROUPS_LINE.search(check_all.read_text(encoding="utf-8")) if check_all.is_file() else None
    known = listed.group(1).split() if listed else []
    try:
        preconditions(git, args.main_ref, args.branches, args.group, known)
    except Refused as error:
        print(f"land: refused: {error}", file=sys.stderr)
        return 2
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    log_dir = (args.log_dir or git.top / "target" / "land" / stamp).resolve()
    log_dir.mkdir(parents=True, exist_ok=True)
    gate = shlex.split(os.environ.get(GATE_ENV) or CHECK_ALL)
    ctx = Context(git, args.main_ref, gate, log_dir, known, args.group)
    status, batch, groups, code = "error", None, [], 1
    try:
        status, batch, groups = land(ctx, args.branches, args.dry_run, args.max_rounds)
        code = 0 if status in ("landed", "dry-run", "nothing-to-land") and not ctx.ejected else 1
    except HostRefused as refusal:
        record = refusal.args[0]
        print(f"⏸ {record['group']} exited {HOST_REFUSAL} (host or resource refusal; log {record['log']}): "
              f"nothing landed and no lane is ejected for it; rerun later", flush=True)
        status, code = "host-refused", HOST_REFUSAL
    except Refused as error:
        print(f"land: refused: {error}", file=sys.stderr)
        status, code = "refused", 2
    except RuntimeError as error:
        print(f"land: {error}", file=sys.stderr)
        status, code = "error", 1
    finally:
        if status != "landed":
            # Leave integration on main so the next run's ancestry check holds.
            git.reset(git.commit(args.main_ref) if git.ancestor("HEAD", args.main_ref) else
                      git.out("merge-base", "HEAD", args.main_ref))
    summarize(ctx, status, batch, groups, log_dir / "summary.json")
    return code


# --- self-test ---------------------------------------------------------------------------------------

FAKE_GATE = r'''
import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ["LAND_TEST_ROOT"])
group = sys.argv[sys.argv.index("--group") + 1]
head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
with open(root / "gate.jsonl", "a") as log:
    log.write(json.dumps({"group": group, "head": head, "service": os.environ.get("AWAKEN_SERVICE_AFFECTED_BASE"),
                          "tier": os.environ.get("AWAKEN_FORMAL_TIER"),
                          "formal": os.environ.get("AWAKEN_FORMAL_AFFECTED_BASE")}) + "\n")
if os.environ.get("FAKE_REFUSE") == group:
    sys.exit(75)
if os.environ.get("FAKE_HOST_FAULT") == group and os.environ.get("AWAKEN_SERVICE_AFFECTED_BASE") != head:
    # A host fault shows only once a suite runs; narrowed to itself, a tip runs none.
    print(f"{group}: the host's substrate is broken")
    sys.exit(1)
if os.path.exists(f"breaks-{group}"):
    print(f"{group} broken by this tip")
    sys.exit(1)
if os.environ.get("FAKE_MOVE_MAIN") and not (root / "moved").exists():
    (root / "moved").touch()
    subprocess.run(["git", "update-ref", "refs/heads/trunk", "refs/heads/upstream"], check=True)
'''


class Fixture:
    """A scratch repository: main is ``trunk`` (checked out nowhere), the writer worktree is on integration."""

    def __init__(self, root: Path):
        self.root, home, hooks = root, root / "home", root / "no-hooks"
        home.mkdir()
        hooks.mkdir()
        (home / ".gitconfig").write_text(
            "[user]\n\tname = Land Test\n\temail = land@example.invalid\n[commit]\n\tgpgsign = false\n"
            f"[core]\n\thooksPath = {hooks}\n[init]\n\tdefaultBranch = trunk\n", encoding="utf-8")
        (root / "gate.py").write_text(FAKE_GATE, encoding="utf-8")
        self.env = environment({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
                                "GIT_CONFIG_NOSYSTEM": "1", "LAND_TEST_ROOT": str(root),
                                GATE_ENV: shlex.join([sys.executable, str(root / "gate.py")])})
        for name in ("FAKE_REFUSE", "FAKE_MOVE_MAIN"):
            self.env.pop(name, None)
        self.repo, self.writer = root / "repo", root / "writer"
        self.repo.mkdir()
        self.git("init", "-q")
        self.write({"shared.txt": "base\n"})
        self.git("commit", "-qm", "base")
        self.base = self.rev("trunk")

    def git(self, *args: str, cwd: Path | None = None, check: bool = True) -> str:
        result = subprocess.run(["git", *args], cwd=cwd or self.repo, env=self.env, capture_output=True, text=True)
        assert not check or result.returncode == 0, (args, result.stderr)
        return result.stdout.strip()

    def rev(self, ref: str, cwd: Path | None = None) -> str:
        return self.git("rev-parse", ref, cwd=cwd)

    def write(self, files: dict[str, str]) -> None:
        for name, text in files.items():
            (self.repo / name).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / name).write_text(text, encoding="utf-8")
        self.git("add", "-A")

    def lane(self, name: str, files: dict[str, str], parent: str = "trunk") -> None:
        self.git("checkout", "-q", "-B", name, parent)
        self.write(files)
        self.git("commit", "-qm", f"{name} change")

    def ready(self) -> None:
        self.git("checkout", "-q", "--detach")
        self.git("branch", INTEGRATION, "trunk")
        self.git("worktree", "add", "-q", str(self.writer), INTEGRATION)
        self.git("config", "awaken.mainWriter", str(self.writer))

    def land(self, *args: str, cwd: Path | None = None, **extra: str) -> subprocess.CompletedProcess:
        command = [sys.executable, str(SELF), "--main-ref", "trunk", "--log-dir", str(self.root / "logs"), *args]
        return subprocess.run(command, cwd=cwd or self.writer, env={**self.env, **extra}, capture_output=True,
                              text=True)

    def runs(self) -> list[dict]:
        log = self.root / "gate.jsonl"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def summary(self) -> dict:
        return json.loads((self.root / "logs" / "summary.json").read_text())

    def has(self, ref: str, path: str) -> bool:
        return subprocess.run(["git", "cat-file", "-e", f"{ref}:{path}"], cwd=self.repo, env=self.env,
                              stderr=subprocess.DEVNULL).returncode == 0

    def settled(self) -> None:
        """Integration equals main and the writer is clean, whatever happened."""
        assert self.rev("HEAD", self.writer) == self.rev("trunk"), "integration was left off main"
        assert not self.git("status", "--porcelain", cwd=self.writer), "writer left dirty"


def explain(result: subprocess.CompletedProcess) -> str:
    return f"exit {result.returncode}\n{result.stdout}\n{result.stderr}"


def case_clean_lanes(fx: Fixture) -> None:
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.lane("lane-b", {"b.txt": "b\n"})
    fx.ready()
    result = fx.land("--group", "static", "--group", "rust", "lane-a", "lane-b")
    assert result.returncode == 0, explain(result)
    tip = fx.rev("trunk")
    assert fx.has(tip, "a.txt") and fx.has(tip, "b.txt"), "B1: both lanes landed"
    assert fx.git("rev-list", "--count", f"{fx.base}..trunk") == "2", "one commit per lane, linear"
    runs = fx.runs()
    assert [run["group"] for run in runs] == ["static", "rust"], f"V1: one run per group: {runs}"
    assert all(run["head"] == tip and run["service"] == fx.base and run["formal"] == fx.base
               and run["tier"] == "pr" for run in runs), runs
    assert [lane["branch"] for lane in fx.summary()["landed"]] == ["lane-a", "lane-b"]
    fx.settled()


def case_conflict(fx: Fixture) -> None:
    fx.lane("lane-a", {"shared.txt": "A\n"})
    fx.lane("lane-c", {"shared.txt": "C\n"})
    fx.lane("lane-b", {"b.txt": "b\n"})
    fx.ready()
    result = fx.land("--group", "static", "lane-a", "lane-c", "lane-b")
    assert result.returncode == 1, explain(result)
    summary = fx.summary()
    assert [(e["branch"], e["paths"]) for e in summary["ejected"]] == [("lane-c", ["shared.txt"])], summary
    assert "conflict" in summary["ejected"][0]["reason"]
    assert [lane["branch"] for lane in summary["landed"]] == ["lane-a", "lane-b"], summary
    assert fx.git("show", "trunk:shared.txt") == "A" and fx.has("trunk", "b.txt"), "B2: the rest landed"
    assert [run["group"] for run in fx.runs()] == ["static"]
    fx.settled()


def case_breaking_lane(fx: Fixture) -> None:
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.lane("lane-x", {"breaks-rust": "x\n"})
    fx.lane("lane-b", {"b.txt": "b\n"})
    fx.ready()
    result = fx.land("--group", "static", "--group", "rust", "lane-a", "lane-x", "lane-b")
    assert result.returncode == 1, explain(result)
    summary = fx.summary()
    assert [e["branch"] for e in summary["ejected"]] == ["lane-x"], summary
    assert "rust" in summary["ejected"][0]["reason"] and Path(summary["ejected"][0]["log"]).is_file()
    assert [lane["branch"] for lane in summary["landed"]] == ["lane-a", "lane-b"], summary
    assert not fx.has("trunk", "breaks-rust") and fx.has("trunk", "a.txt") and fx.has("trunk", "b.txt")
    groups = [run["group"] for run in fx.runs()]
    # round 1: static, rust (fails); bisection: rust on prefixes 1 and 2 only; round 2: static, rust.
    assert groups == ["static", "rust", "rust", "rust", "static", "rust"], f"V3: {groups}"
    assert all(run["head"] == fx.rev("trunk") for run in fx.runs()[-2:]), "re-verified the landed tip"
    fx.settled()


def case_host_refusal(fx: Fixture) -> None:
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.lane("lane-x", {"breaks-static": "x\n"})
    fx.ready()
    result = fx.land("--group", "rust", "--group", "static", "lane-a", "lane-x", FAKE_REFUSE="rust")
    assert result.returncode == HOST_REFUSAL, explain(result)
    assert "rerun later" in result.stdout, result.stdout
    assert fx.rev("trunk") == fx.base, "V2: main unchanged"
    summary = fx.summary()
    assert summary["status"] == "host-refused" and not summary["ejected"] and not summary["landed"], summary
    assert [run["group"] for run in fx.runs()] == ["rust"], "V2: no further group and no bisection"
    fx.settled()


def case_main_moves(fx: Fixture) -> None:
    fx.lane("upstream", {"u.txt": "u\n"})
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    result = fx.land("--group", "static", "--group", "rust", "lane-a", FAKE_MOVE_MAIN="1")
    assert result.returncode == 0, explain(result)
    upstream = fx.rev("upstream")
    fx.git("merge-base", "--is-ancestor", upstream, "trunk")  # V4: landed on the new main
    assert fx.has("trunk", "u.txt") and fx.has("trunk", "a.txt")
    runs = fx.runs()
    assert [run["group"] for run in runs] == ["static", "rust", "static", "rust"], runs
    assert [run["service"] for run in runs] == [fx.base, fx.base, upstream, upstream], "rebuilt on the new main"
    assert runs[-1]["head"] == fx.rev("trunk")
    fx.settled()


def case_round_limit(fx: Fixture) -> None:
    fx.lane("upstream", {"u.txt": "u\n"})
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    result = fx.land("--max-rounds", "1", "--group", "static", "lane-a", FAKE_MOVE_MAIN="1")
    assert result.returncode == 1 and fx.summary()["status"] == "round-limit", explain(result)
    assert fx.rev("trunk") == fx.rev("upstream"), "V6: nothing landed"
    fx.settled()


def case_main_fails(fx: Fixture) -> None:
    fx.write({"breaks-rust": "already\n"})
    fx.git("commit", "-qm", "broken main")
    broken = fx.rev("trunk")
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    result = fx.land("--group", "rust", "lane-a")
    assert result.returncode == 1 and fx.summary()["status"] == "main-fails", explain(result)
    assert not fx.summary()["ejected"] and fx.rev("trunk") == broken, "V5: nobody ejected for main's failure"
    fx.settled()


def case_shared_host_fault(fx: Fixture) -> None:
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    result = fx.land("--group", "postgres", "lane-a", FAKE_HOST_FAULT="postgres")
    assert result.returncode == 1 and fx.summary()["status"] == "main-fails", explain(result)
    assert not fx.summary()["ejected"], "V7: lane 1 kept when main fails the same group un-narrowed"
    runs = fx.runs()
    assert [run["service"] for run in runs] == [fx.base, ""], f"V7: main ran un-narrowed: {runs}"
    assert runs[-1]["head"] == fx.base and runs[-1]["formal"] == "", runs[-1]
    fx.settled()


def case_absorbed(fx: Fixture) -> None:
    fx.lane("lane-dup", {"u.txt": "u\n"})
    fx.git("checkout", "-q", "trunk")
    fx.write({"u.txt": "u\n"})
    fx.git("commit", "-qm", "same change, landed earlier")
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    result = fx.land("--group", "static", "lane-dup", "lane-a")
    assert result.returncode == 0, explain(result)
    summary = fx.summary()
    assert summary["absorbed"] == ["lane-dup"] and not summary["ejected"], f"B3: {summary}"
    assert [lane["branch"] for lane in summary["landed"]] == ["lane-a"], summary
    fx.settled()


def case_refusals(fx: Fixture) -> None:
    fx.lane("lane-a", {"a.txt": "a\n"})
    fx.ready()
    outside = fx.land("lane-a", cwd=fx.repo)
    assert outside.returncode == 2 and "awaken.mainWriter" in outside.stderr, explain(outside)
    unknown = fx.land("no-such-lane")
    assert unknown.returncode == 2 and "no-such-lane" in unknown.stderr, explain(unknown)
    (fx.writer / "stray.txt").write_text("x\n")
    dirty = fx.land("lane-a")
    assert dirty.returncode == 2 and "clean" in dirty.stderr, explain(dirty)
    (fx.writer / "stray.txt").unlink()
    fx.git("checkout", "-q", "--detach", cwd=fx.writer)
    detached = fx.land("lane-a")
    assert detached.returncode == 2 and INTEGRATION in detached.stderr, explain(detached)
    fx.git("checkout", "-q", INTEGRATION, cwd=fx.writer)
    fx.git("commit", "-q", "--allow-empty", "-m", "unlanded", cwd=fx.writer)
    ahead = fx.land("lane-a")
    assert ahead.returncode == 2 and "ancestor" in ahead.stderr, explain(ahead)
    assert fx.rev("trunk") == fx.base and not fx.runs(), "W1: nothing ran and main is unchanged"


def case_dry_run(fx: Fixture) -> None:
    fx.lane("lane-a", {"docs/a.md": "a\n", "shared.txt": "A\n"})
    fx.lane("lane-c", {"shared.txt": "C\n"})
    fx.ready()
    result = fx.land("--dry-run", "lane-a", "lane-c")
    assert result.returncode == 1, explain(result)  # lane-c is ejected for its conflict
    assert "docs: docs/a.md (matches docs/**)" in result.stdout and "static: always" in result.stdout, result.stdout
    assert "eject lane-c: conflict in shared.txt" in result.stdout, result.stdout
    assert not fx.runs() and fx.rev("trunk") == fx.base, "dry run ran no gate and moved nothing"
    fx.settled()


def case_selection() -> None:
    """The table, against this repository's own check-all.sh and Rust inputs."""
    text = (SELF.parent / "check-all.sh").read_text(encoding="utf-8")
    known = GROUPS_LINE.search(text).group(1).split()
    pick = lambda *paths, base=text, tip=text: [g for g, _ in select_groups(list(paths), tip, base, known)]
    assert pick("docs/design/x.md") == ["static", "docs"], pick("docs/design/x.md")
    assert pick("crates/a/src/lib.rs") == ["static", *RUST_GROUPS], pick("crates/a/src/lib.rs")
    assert pick("crates/a/migrations/1.sql")[1:] == list(RUST_GROUPS), "migrations are Rust inputs"
    assert pick("web/src/a.ts") == ["static", "frontend"]
    assert "postgres" in pick("scripts/ci/pg_tests.sh") and "rust" not in pick("scripts/ci/pg_tests.sh")
    assert pick(".github/workflows/x.yml") == ["static"], pick(".github/workflows/x.yml")
    added = text.replace('run docs "documentation"', 'run static "new" python3 x.py\nrun docs "documentation"')
    assert pick(CHECK_ALL, tip=added) == ["static"], pick(CHECK_ALL, tip=added)
    harness = text.replace("set -uo pipefail", "set -uo pipefail\numask 022")
    assert pick(CHECK_ALL, tip=harness) == known, "a harness line selects every group"


def self_test() -> None:
    cases = [case_clean_lanes, case_conflict, case_breaking_lane, case_host_refusal, case_main_moves,
             case_round_limit, case_main_fails, case_shared_host_fault, case_absorbed, case_refusals, case_dry_run]
    for case in cases:
        with tempfile.TemporaryDirectory(prefix="land-") as scratch:
            case(Fixture(Path(scratch)))
    case_selection()
    print(f"land self-test: {len(cases) + 1} cases hold")


if __name__ == "__main__":
    sys.exit(main())
