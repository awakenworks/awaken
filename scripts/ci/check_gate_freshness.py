#!/usr/bin/env python3
"""Keep the full gate's last green run visible, and refuse a stale one.

Usage:
  check_gate_freshness.py                       report; never fails on staleness
  check_gate_freshness.py --require-fresh       fail when the record has expired
  check_gate_freshness.py --record --gates ... --expected-commit SHA
                                              record the clean tested candidate
  check_gate_freshness.py --self-test           prove the decision table

Why this exists
---------------
The full gate — ``check-rust.sh --full``, every ``check-all.sh`` group, and
``check_formal.sh --require-tools`` — takes hours, so in practice it is rarely
run end to end. ``main`` had therefore never seen one complete, and four
unrelated defects piled up behind it, each hiding the next: a missing solver, a
revoked qualification receipt, a mutation configuration inheriting a property
that no longer existed, a trace renderer keying generations without the digest.

None of that was invisible because it was subtle. It was invisible because
nothing recorded *when the gate last actually passed*, so nobody could tell that
the answer was "never". ``scripts/ci/gate_record.json`` is that record, written
only by a run that passed everything, and this checker is what notices when
``main`` has walked away from it.

The record
----------
It lives beside this checker, in ``scripts/ci`` rather than ``formal/``, because
it covers every group — static, docs, rust, api, contracts, formal, reliability,
postgres, kubernetes, k3d, frontend, e2e, sandbox — and ``formal/`` holds only
formal evidence. Its integrity is checked, not assumed: the commit it names must
exist in this repository and must be an ancestor of ``main``, so a record cannot
bless work that never landed, or a commit that was rewritten away.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

# Input set: the record, the group list a full run must cover, and the manifest
# whose tool versions a record carries.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/gate_record.json",
    "scripts/ci/check-all.sh",
    "scripts/ci/gate_tools.json",
)

ROOT = Path(__file__).resolve().parents[2]
RECORD = Path(__file__).resolve().parent / "gate_record.json"
CHECK_ALL = Path(__file__).resolve().parent / "check-all.sh"

# The freshness policy, in one place. docs/adr/0080 states it in prose and cites
# these four names; nothing else in the repository may hold a second copy.
WARN_COMMITS = 20
FAIL_COMMITS = 50
WARN_DAYS = 3
FAIL_DAYS = 7

MAIN_CANDIDATES = ("main", "origin/main", "refs/heads/main")
SHA = re.compile(r"^[0-9a-f]{40}$")

FRESH, WARN, EXPIRED = "fresh", "warn", "expired"


def unambient_environment() -> dict[str, str]:
    """The environment with git's ambient repository selection removed.

    ``GIT_DIR`` and its relatives outrank ``-C``, and a hook runner exports them.
    A ``git init`` that inherits them does not create the repository it was
    pointed at: it re-initializes the *caller's* repository, and because the
    inherited directory has no worktree beside it, that sets ``core.bare``. This
    checker creates a throwaway repository in its self-test, so it must not
    inherit that selection — the second time this class of defect was the whole
    point of the work it was written for.
    """

    return {
        name: value
        for name, value in os.environ.items()
        if not name.startswith("GIT_") or name in ("GIT_CONFIG_NOSYSTEM",)
    }


def git(repo: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *arguments],
        capture_output=True,
        text=True,
        timeout=300,
        env=unambient_environment(),
    )


def main_reference(repo: Path) -> str | None:
    """Whichever ``main`` this checkout has; ``None`` when it has none."""

    for candidate in MAIN_CANDIDATES:
        if git(repo, "rev-parse", "--verify", "--quiet", f"{candidate}^{{commit}}").returncode == 0:
            return candidate
    return None


def verdict(commits_behind: int, age_days: float) -> str:
    """The one place the thresholds decide anything.

    | Rule | commits behind | age        | verdict |
    |------|----------------|------------|---------|
    | V1   | <= 20          | <= 3 days  | fresh   |
    | V2   | > 20           | any        | warn    |
    | V3   | any            | > 3 days   | warn    |
    | V4   | > 50           | any        | expired |
    | V5   | any            | > 7 days   | expired |
    """

    if commits_behind > FAIL_COMMITS or age_days > FAIL_DAYS:
        return EXPIRED
    if commits_behind > WARN_COMMITS or age_days > WARN_DAYS:
        return WARN
    return FRESH


def all_groups() -> list[str]:
    text = CHECK_ALL.read_text(encoding="utf-8")
    match = re.search(r"^groups=\(([^)]*)\)", text, re.MULTILINE)
    if not match:
        raise SystemExit("check_gate_freshness: check-all.sh declares no group list")
    return match.group(1).split()


def integrity_problems(record: dict, repo: Path) -> list[str]:
    """Everything about a record that can be wrong regardless of how old it is."""

    problems: list[str] = []
    commit = record.get("commit", "")
    if not isinstance(commit, str) or not SHA.match(commit):
        problems.append(f"the recorded commit {commit!r} is not a full 40-character sha")
        return problems
    if git(repo, "cat-file", "-e", f"{commit}^{{commit}}").returncode != 0:
        problems.append(
            f"the recorded commit {commit[:12]} does not exist in this repository; "
            "the record blesses work that was rewritten or never landed"
        )
        return problems
    reference = main_reference(repo)
    if reference is None:
        problems.append("this checkout has no main reference, so the record cannot be verified")
        return problems
    if git(repo, "merge-base", "--is-ancestor", commit, reference).returncode != 0:
        problems.append(
            f"the recorded commit {commit[:12]} is not an ancestor of {reference}; "
            "a record may only bless a commit that is on main"
        )
    recorded_at = record.get("recorded_at", "")
    try:
        stamp = datetime.fromisoformat(str(recorded_at).replace("Z", "+00:00"))
    except ValueError:
        problems.append(f"the recorded timestamp {recorded_at!r} is not an ISO-8601 instant")
        return problems
    if stamp.tzinfo is None:
        problems.append(f"the recorded timestamp {recorded_at!r} carries no time zone")
    elif stamp > datetime.now(timezone.utc) + timedelta(minutes=5):
        problems.append(f"the recorded timestamp {recorded_at} is in the future")
    missing = [field for field in ("gates", "tools", "runner") if not record.get(field)]
    for field in missing:
        problems.append(f"the record carries no {field}")
    recorded_gates = record.get("gates") or []
    absent = [group for group in all_groups() if group not in recorded_gates]
    if absent:
        problems.append(
            "the record was written by a partial run; it does not cover: " + " ".join(absent)
        )
    return problems


def distance(record: dict, repo: Path) -> tuple[int, float, str]:
    reference = main_reference(repo) or "HEAD"
    counted = git(repo, "rev-list", "--count", f"{record['commit']}..{reference}")
    behind = int(counted.stdout.strip() or "0") if counted.returncode == 0 else 0
    stamp = datetime.fromisoformat(str(record["recorded_at"]).replace("Z", "+00:00"))
    age = (datetime.now(timezone.utc) - stamp).total_seconds() / 86400.0
    return behind, age, reference


def tool_versions() -> dict[str, str]:
    """What this machine actually ran the gate with, from the tool manifest."""

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import gate_tools  # noqa: PLC0415 - it owns the manifest

    manifest = gate_tools.load()
    versions: dict[str, str] = {}
    for tool in manifest["tools"]:
        found = gate_tools.locate(tool)
        if not found.usable:
            continue
        pin = tool.get("version", {})
        command = pin.get("command")
        reported = "present"
        if command:
            executable = found.where if found.where and Path(found.where).exists() else command[0]
            try:
                result = subprocess.run(
                    [executable, *command[1:]], capture_output=True, text=True, timeout=120
                )
                lines = (result.stdout or result.stderr).strip().splitlines()
                reported = lines[0] if lines else "present"
            except (OSError, subprocess.SubprocessError):
                reported = "present"
        elif pin.get("expect"):
            reported = str(pin["expect"])
        versions[tool["name"]] = reported
    return versions


def source_matches_candidate(repo: Path, expected_commit: str) -> bool:
    """One source fence reused before tool observations and record publication."""
    head = git(repo, "rev-parse", "HEAD")
    dirty = git(repo, "status", "--porcelain", "--untracked-files=all", "--", ".",
                ":(exclude)scripts/ci/gate_record.json")
    return (bool(SHA.fullmatch(expected_commit)) and head.returncode == 0
            and head.stdout.strip() == expected_commit and dirty.returncode == 0
            and not dirty.stdout.strip())


def write_record(repo: Path, gates: list[str], path: Path, expected_commit: str = "") -> int:
    """Record the same clean candidate, refusing partial or changed-source runs."""

    expected = all_groups()
    absent = [group for group in expected if group not in gates]
    if absent:
        print(
            "refusing to record a partial run; these groups did not run: " + " ".join(absent),
            file=sys.stderr,
        )
        return 1
    head = git(repo, "rev-parse", "HEAD").stdout.strip()
    if not SHA.match(head):
        print("refusing to record: HEAD is not a commit", file=sys.stderr)
        return 1
    if not source_matches_candidate(repo, expected_commit):
        print("refusing to record: candidate SHA changed, is missing, or source is not clean", file=sys.stderr)
        return 1
    document = {
        "version": 1,
        "commit": head,
        "recorded_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "gates": sorted(set(gates)),
        "runner": "scripts/ci/check-all.sh --full-gate --record",
        "host": os.uname().nodename,
        "tools": tool_versions(),
    }
    if not source_matches_candidate(repo, expected_commit):
        print("refusing to record: source changed while tool evidence was collected", file=sys.stderr)
        return 1
    path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"recorded the full gate at {head[:12]} in {path}")
    print("commit this record: a green run nobody can see is a run nobody trusts")
    return 0


def report(repo: Path, path: Path, strict: bool) -> int:
    """Cause/effect for one record:

    | Rule | record                                   | default | --require-fresh |
    |------|------------------------------------------|---------|-----------------|
    | R1   | fresh                                    | accept  | accept          |
    | R2   | past a warn threshold                    | warn    | warn            |
    | R3   | past a fail threshold, or absent         | warn    | reject          |
    | R4   | names a commit that is gone or off main  | reject  | reject          |
    | R5   | written by a partial run                 | reject  | reject          |
    """

    if not path.is_file():
        print(
            f"warning: {path.name} does not exist: the full gate has never been recorded here.\n"
            "  run: scripts/ci/check-all.sh --full-gate --record",
            file=sys.stderr,
        )
        return 1 if strict else 0
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        print(f"ERROR: {path.name} is not readable JSON: {error}", file=sys.stderr)
        return 1
    problems = integrity_problems(record, repo)
    if problems:
        print(f"ERROR: the full-gate record is not trustworthy ({path.name}):", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    behind, age, reference = distance(record, repo)
    state = verdict(behind, age)
    summary = (
        f"full gate last passed at {record['commit'][:12]} ({record['recorded_at']}); "
        f"{reference} is {behind} commit(s) ahead, {age:.1f} day(s) ago"
    )
    if state == FRESH:
        print(f"OK - {summary}")
        return 0
    escalation = (
        f"warn at {WARN_COMMITS} commits / {WARN_DAYS} days, "
        f"fail at {FAIL_COMMITS} commits / {FAIL_DAYS} days"
    )
    label = "warning" if state == WARN else "warning: EXPIRED"
    print(
        f"{label}: {summary}\n"
        f"  policy ({escalation}) — docs/adr/0080\n"
        f"  refresh it: scripts/ci/check-all.sh --full-gate --record",
        file=sys.stderr,
    )
    return 1 if (strict and state == EXPIRED) else 0


# ---------------------------------------------------------------- self-test


def _quiet(call) -> int:
    """Run one probe for its verdict only; its report belongs to a real caller."""

    import contextlib
    import io

    sink = io.StringIO()
    with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        return call()


def _throwaway_repository(directory: Path) -> Path:
    repo = directory / "repo"
    repo.mkdir()
    environment = {
        **unambient_environment(),
        "GIT_AUTHOR_NAME": "gate",
        "GIT_AUTHOR_EMAIL": "gate@example.invalid",
        "GIT_COMMITTER_NAME": "gate",
        "GIT_COMMITTER_EMAIL": "gate@example.invalid",
    }

    def run(*arguments: str) -> None:
        subprocess.run(
            ["git", "-C", str(repo), *arguments],
            check=True,
            capture_output=True,
            text=True,
            env=environment,
        )

    run("init", "--initial-branch=main", ".")
    # Fail loudly rather than proceed if the repository landed anywhere else:
    # an init that escaped this directory has already edited somebody's
    # repository, and every assertion after it would be about the wrong one.
    located = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "--absolute-git-dir"],
        capture_output=True, text=True, env=environment, check=True,
    ).stdout.strip()
    if Path(located).resolve() != (repo / ".git").resolve():
        raise SystemExit(
            f"check_gate_freshness self-test: the throwaway repository was created at "
            f"{located}, not inside {repo}; refusing to touch it"
        )
    for index in range(3):
        (repo / f"file{index}").write_text(str(index), encoding="utf-8")
        run("add", ".")
        run("commit", "-m", f"commit {index}")
    return repo


def _record(repo: Path, commit: str, days_old: float, gates: list[str]) -> dict:
    stamp = datetime.now(timezone.utc) - timedelta(days=days_old)
    return {
        "version": 1,
        "commit": commit,
        "recorded_at": stamp.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "gates": gates,
        "runner": "scripts/ci/check-all.sh --full-gate --record",
        "tools": {"cargo": "present"},
    }


def self_test() -> int:
    """Prove the table above, including the negative probes.

    | Rule | cause                                       | effect                     |
    |------|---------------------------------------------|----------------------------|
    | V1-5 | commit distance and age                      | fresh / warn / expired     |
    | I1   | a record naming a commit this repo lacks     | integrity failure          |
    | I2   | a record naming a commit that is off main    | integrity failure          |
    | I3   | a record listing fewer groups than check-all | integrity failure          |
    | I4   | a record on main, complete and recent        | no integrity problem       |
    | S1   | an expired record, default mode              | warns, accepts             |
    | S2   | an expired record, --require-fresh           | rejects                    |
    | S3   | no record at all, --require-fresh            | rejects                    |
    | W1   | --record with a partial group list           | refuses to write           |
    | G1   | an ambient GIT_DIR, as a hook runner exports | never reaches the fixture  |
    """

    problems: list[str] = []
    groups = all_groups()

    table = [
        (0, 0.0, FRESH),
        (WARN_COMMITS, float(WARN_DAYS), FRESH),
        (WARN_COMMITS + 1, 0.0, WARN),
        (0, WARN_DAYS + 0.5, WARN),
        (FAIL_COMMITS + 1, 0.0, EXPIRED),
        (0, FAIL_DAYS + 0.5, EXPIRED),
    ]
    for behind, age, expected in table:
        actual = verdict(behind, age)
        if actual != expected:
            problems.append(f"V {behind} commits / {age} days gave {actual}, expected {expected}")

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        # G1: build the fixture under exactly the ambient selection a hook runner
        # exports. `git init` obeys GIT_DIR over -C, so an inheriting self-test
        # re-initializes the caller's repository and flips its core.bare. The
        # decoy below must come back untouched.
        decoy = root / "decoy"
        decoy.mkdir()
        previous = os.environ.get("GIT_DIR")
        os.environ["GIT_DIR"] = str(decoy)
        try:
            repo = _throwaway_repository(root)
        finally:
            if previous is None:
                os.environ.pop("GIT_DIR", None)
            else:
                os.environ["GIT_DIR"] = previous
        if any(decoy.iterdir()):
            problems.append("G1 an ambient GIT_DIR captured the throwaway repository")
        head = git(repo, "rev-parse", "HEAD").stdout.strip()
        first = git(repo, "rev-list", "--max-parents=0", "HEAD").stdout.strip()
        git(repo, "checkout", "-q", "-b", "side", first)
        (repo / "side").write_text("side", encoding="utf-8")
        git(repo, "add", ".")
        git(
            repo, "-c", "user.name=gate", "-c", "user.email=g@e.invalid",
            "commit", "-m", "off main",
        )
        off_main = git(repo, "rev-parse", "HEAD").stdout.strip()
        git(repo, "checkout", "-q", "main")

        if integrity_problems(_record(repo, head, 0.0, groups), repo):
            problems.append("I4 a complete record on main was rejected")
        if not integrity_problems(_record(repo, "0" * 40, 0.0, groups), repo):
            problems.append("I1 a record naming an absent commit was accepted")
        if not integrity_problems(_record(repo, off_main, 0.0, groups), repo):
            problems.append("I2 a record naming a commit off main was accepted")
        if not integrity_problems(_record(repo, head, 0.0, groups[:2]), repo):
            problems.append("I3 a record written by a partial run was accepted")

        expired = root / "expired.json"
        expired.write_text(
            json.dumps(_record(repo, head, FAIL_DAYS + 1, groups)), encoding="utf-8"
        )
        if _quiet(lambda: report(repo, expired, strict=False)) != 0:
            problems.append("S1 an expired record must only warn by default")
        if _quiet(lambda: report(repo, expired, strict=True)) != 1:
            problems.append("S2 --require-fresh must reject an expired record")
        if _quiet(lambda: report(repo, root / "absent.json", strict=True)) != 1:
            problems.append("S3 --require-fresh must reject a machine with no record")
        if _quiet(lambda: report(repo, root / "absent.json", strict=False)) != 0:
            problems.append("S3 a missing record must only warn by default")
        if _quiet(lambda: write_record(repo, groups[:1], root / "partial.json")) != 1:
            problems.append("W1 a partial run was allowed to bless the record")
        if (root / "partial.json").exists():
            problems.append("W1 a refused record still wrote a file")

        # Source cause/effect table: W2 complete gates but no candidate binding,
        # wrong SHA, dirty tracked source or new untracked source => refuse
        # without a receipt. W3 the same clean exact candidate => one receipt.
        # W4 source changes during tool-version observation => refuse. Main
        # ancestry remains the separate I2 integrity rule, unchanged here.
        from unittest.mock import patch
        def record_candidate(name: str, candidate: str) -> int:
            return _quiet(lambda: write_record(repo, groups, root / name, candidate))
        with patch(__name__ + ".tool_versions", return_value={"cargo": "test-fixture"}):
            for name, candidate in (("missing.json", ""), ("wrong.json", first)):
                if record_candidate(name, candidate) != 1 or (root / name).exists():
                    problems.append(f"W2 {name} recorded an unbound candidate")
            tracked = repo / "file0"
            original = tracked.read_text()
            tracked.write_text("changed source")
            if record_candidate("dirty.json", head) != 1 or (root / "dirty.json").exists():
                problems.append("W2 dirty tracked source recorded")
            tracked.write_text(original)
            untracked = repo / "new-source.rs"
            untracked.write_text("new source")
            if record_candidate("untracked.json", head) != 1 or (root / "untracked.json").exists():
                problems.append("W2 untracked source recorded")
            untracked.unlink()
            if record_candidate("exact.json", head) != 0:
                problems.append("W3 exact clean candidate refused")
            elif json.loads((root / "exact.json").read_text())["commit"] != head:
                problems.append("W3 record drifted from tested SHA")
        def mutate_during_tool_observation() -> dict[str, str]:
            tracked.write_text("changed during tool observation")
            return {"cargo": "test-fixture"}
        with patch(__name__ + ".tool_versions", side_effect=mutate_during_tool_observation):
            if record_candidate("raced.json", head) != 1 or (root / "raced.json").exists():
                problems.append("W4 source race recorded")
        tracked.write_text(original)

    if problems:
        print("check_gate_freshness self-test:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(
        "check_gate_freshness self-test: the threshold table, the integrity rules and "
        "the partial-run refusal all hold"
    )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--require-fresh", action="store_true")
    parser.add_argument("--record", action="store_true")
    parser.add_argument("--gates", default="")
    parser.add_argument("--expected-commit", default="")
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args(argv)

    if arguments.self_test:
        return self_test()
    if arguments.record:
        return write_record(ROOT, arguments.gates.split(), RECORD, arguments.expected_commit)
    return report(ROOT, RECORD, arguments.require_fresh)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
