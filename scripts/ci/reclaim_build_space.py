#!/usr/bin/env python3
"""Report, and on request reclaim, idle build output across every checkout.

Usage:
  reclaim_build_space.py                  dry run: what it would free, per category
  reclaim_build_space.py --apply          actually remove the reclaimable paths
  reclaim_build_space.py --summary        one line per category, for doctor.sh
  reclaim_build_space.py --self-test      prove the decision table

Why this exists
---------------
This machine filled up twice in one session. 152 of 219 checkouts carried an
in-tree ``target/`` totalling 509G against roughly 1.1G of source each; the
shared cargo intermediates had grown to 1.4T; a stale tree held another 129G.
A full filesystem does not announce itself as a full filesystem: it reached CBMC
as ``os error 28``, truncated a GOTO binary, and crashed the backend with
``SIGABRT`` minutes later, which reads as a proof failure.

Build output is derived and can always be rebuilt. A checkout, its history and
its uncommitted changes cannot, and nothing here touches them: this removes
``target`` directories and shared intermediate entries, never a source tree.

The thresholds, the roots and the safety rules live in the
``build_space_reclamation`` section of ``scripts/ci/gate_tools.json``, beside the
disk minimums the gate stages precheck against, because they are the same
policy seen from two ends: how much room a stage needs, and where room comes
from. ``docs/adr/0080`` states it in prose.

Liveness is read from ``/proc/<pid>/cwd`` and ``/proc/<pid>/environ``. It is
never inferred from a process-name pattern: a pattern match finds the shell that
is asking the question as readily as the build it is asking about.
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

# Input set: the policy section and the module that reads the manifest.
INPUT_GLOBS: tuple[str, ...] = ("scripts/ci/gate_tools.json", "scripts/ci/gate_tools.py")

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate_tools  # noqa: E402 - it owns the manifest

KEPT_IN_USE = "kept: in use"
KEPT_YOUNG = "kept: recently built"
KEPT_PROTECTED = "kept: not build output"
RECLAIMABLE = "reclaimable"

WORKTREE_TARGET = "worktree-target"
DETACHED_TARGET = "detached-target"
SHARED_INTERMEDIATE = "shared-intermediate"

# A directory is cargo build output when it carries one of these. `du` of the
# whole tree is never needed to decide that.
CARGO_MARKERS = ("CACHEDIR.TAG", ".rustc_info.json", "debug", "release")


@dataclass
class Policy:
    idle_days: dict[str, int]
    roots: dict[str, list[Path]]
    safety_rules: list[str]
    needs_a_decision: list[str]


@dataclass
class Candidate:
    category: str
    path: Path
    verdict: str
    idle_days: float
    size_bytes: int | None = None
    detail: str = ""


def policy(manifest: dict | None = None) -> Policy:
    section = (manifest or gate_tools.load())["build_space_reclamation"]

    def roots(key: str) -> list[Path]:
        """Declared roots that exist here. A `*` expands, so one pattern can
        name every per-repository worktree home without listing each."""

        found: list[Path] = []
        for raw in section[key]:
            expanded = gate_tools.expand(raw)
            matches = sorted(glob.glob(expanded)) if "*" in expanded else [expanded]
            for match in matches:
                path = Path(match)
                if path.is_dir() and path not in found:
                    found.append(path)
        return found

    return Policy(
        idle_days={
            WORKTREE_TARGET: section["worktree_target_idle_days"],
            DETACHED_TARGET: section["detached_target_idle_days"],
            SHARED_INTERMEDIATE: section["shared_intermediate_idle_days"],
        },
        roots={
            WORKTREE_TARGET: roots("worktree_roots"),
            DETACHED_TARGET: roots("detached_target_roots"),
            SHARED_INTERMEDIATE: roots("shared_intermediate_roots"),
        },
        safety_rules=section["safety_rules"],
        needs_a_decision=section["needs_a_decision_instead"],
    )


# ---------------------------------------------------------------- liveness


def live_paths(proc: Path = Path("/proc")) -> set[Path]:
    """Directories a live process is working in or building into.

    Read from ``/proc``: each process's cwd, and the two environment names that
    point a cargo build at a target directory outside its checkout. A detached
    target directory is protected by the second of these, because the building
    process's cwd is its checkout rather than its output.
    """

    paths: set[Path] = set()
    if not proc.is_dir():
        return paths
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            paths.add(Path(os.readlink(entry / "cwd")))
        except OSError:
            pass
        try:
            raw = (entry / "environ").read_bytes()
        except OSError:
            continue
        for item in raw.split(b"\0"):
            name, separator, value = item.partition(b"=")
            if not separator:
                continue
            if name.decode("utf-8", "replace") in (
                "CARGO_TARGET_DIR",
                "CARGO_BUILD_BUILD_DIR",
            ):
                text = value.decode("utf-8", "replace").strip()
                if text:
                    paths.add(Path(text))
    return paths


def in_use(path: Path, live: set[Path]) -> bool:
    """True when a live process is at, inside, or building into ``path``."""

    for candidate in live:
        if candidate == path:
            return True
        if path in candidate.parents or candidate in path.parents:
            # A cwd inside the path protects it; so does a cwd whose checkout
            # this path belongs to, which the caller passes as the checkout root.
            if path in candidate.parents:
                return True
    return False


# ---------------------------------------------------------------- inspection


def newest_modification(path: Path, depth: int = 2) -> float:
    """The newest mtime in the first ``depth`` levels below ``path``.

    A build touches fingerprints deep inside ``target`` without changing the
    modification time of ``target`` itself, so the directory alone would report
    an active build as idle.
    """

    newest = 0.0
    stack: list[tuple[Path, int]] = [(path, 0)]
    while stack:
        current, level = stack.pop()
        try:
            newest = max(newest, current.stat().st_mtime)
        except OSError:
            continue
        if level >= depth:
            continue
        try:
            for entry in os.scandir(current):
                try:
                    newest = max(newest, entry.stat(follow_symlinks=False).st_mtime)
                    if entry.is_dir(follow_symlinks=False):
                        stack.append((Path(entry.path), level + 1))
                except OSError:
                    continue
        except OSError:
            continue
    return newest


def idle_days(path: Path, now: float | None = None) -> float:
    reference = time.time() if now is None else now
    newest = newest_modification(path)
    return max(0.0, (reference - newest) / 86400.0) if newest else 0.0


def is_checkout(path: Path) -> bool:
    """A git worktree or repository — never removable, whatever its age."""

    return (path / ".git").exists() or (path / "HEAD").is_file() and (path / "objects").is_dir()


def looks_like_cargo_target(path: Path) -> bool:
    return path.is_dir() and any((path / marker).exists() for marker in CARGO_MARKERS)


def directory_size(path: Path, timeout_seconds: float) -> int | None:
    """Bytes on disk, via ``du``; ``None`` when it did not finish in time."""

    try:
        result = subprocess.run(
            ["du", "-sxb", str(path)],
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0 or not result.stdout.strip():
        return None
    try:
        return int(result.stdout.split()[0])
    except (ValueError, IndexError):
        return None


# ---------------------------------------------------------------- the decision


def classify(
    category: str,
    path: Path,
    owner: Path,
    limit_days: int,
    live: set[Path],
    now: float | None = None,
) -> Candidate:
    """The one decision table.

    | Rule | path                                    | effect            |
    |------|-----------------------------------------|-------------------|
    | C1   | a checkout, a .git path, or not build output | kept: protected |
    | C2   | a live process is in it, or builds into it   | kept: in use    |
    | C3   | its owning checkout is in use                | kept: in use    |
    | C4   | modified within the threshold                | kept: young     |
    | C5   | build output, idle, and nobody is in it      | reclaimable     |
    """

    if path.name == ".git" or is_checkout(path) or not looks_like_cargo_target(path):
        return Candidate(category, path, KEPT_PROTECTED, 0.0, detail="not build output")
    if in_use(path, live) or any(path == candidate for candidate in live):
        return Candidate(category, path, KEPT_IN_USE, 0.0, detail="a live process is building here")
    if owner != path and (owner in live or in_use(owner, live)):
        return Candidate(
            category, path, KEPT_IN_USE, 0.0, detail=f"a live process is working in {owner}"
        )
    age = idle_days(path, now)
    if age < limit_days:
        return Candidate(category, path, KEPT_YOUNG, age, detail=f"younger than {limit_days}d")
    return Candidate(category, path, RECLAIMABLE, age)


def scan(config: Policy, live: set[Path] | None = None, now: float | None = None) -> list[Candidate]:
    known = live if live is not None else live_paths()
    shared_roots = [root.resolve() for root in config.roots[SHARED_INTERMEDIATE]]
    found: list[Candidate] = []

    for root in config.roots[WORKTREE_TARGET]:
        for checkout in sorted(root.iterdir()) if root.is_dir() else []:
            if not checkout.is_dir() or not is_checkout(checkout):
                continue
            target = checkout / "target"
            if target.is_dir():
                found.append(
                    classify(
                        WORKTREE_TARGET, target, checkout,
                        config.idle_days[WORKTREE_TARGET], known, now,
                    )
                )

    for root in config.roots[DETACHED_TARGET]:
        for entry in sorted(root.iterdir()) if root.is_dir() else []:
            if not entry.is_dir() or entry.resolve() in shared_roots:
                continue
            found.append(
                classify(
                    DETACHED_TARGET, entry, entry,
                    config.idle_days[DETACHED_TARGET], known, now,
                )
            )

    for root in config.roots[SHARED_INTERMEDIATE]:
        for entry in sorted(root.iterdir()) if root.is_dir() else []:
            if not entry.is_dir():
                continue
            candidate = classify(
                SHARED_INTERMEDIATE, entry, entry,
                config.idle_days[SHARED_INTERMEDIATE], known, now,
            )
            # An intermediates entry is a content-addressed blob store rather
            # than a cargo target layout, so the target-shape test does not
            # apply to it; only the checkout and liveness guards do.
            if candidate.verdict == KEPT_PROTECTED and not is_checkout(entry):
                candidate = Candidate(
                    SHARED_INTERMEDIATE, entry,
                    RECLAIMABLE if idle_days(entry, now) >= config.idle_days[SHARED_INTERMEDIATE]
                    else KEPT_YOUNG,
                    idle_days(entry, now),
                )
                if in_use(entry, known) or entry in known:
                    candidate = Candidate(
                        SHARED_INTERMEDIATE, entry, KEPT_IN_USE, 0.0,
                        detail="a live process is building here",
                    )
            found.append(candidate)
    return found


# ---------------------------------------------------------------- reporting


def human(size: int | None) -> str:
    if size is None:
        return "size unknown"
    value = float(size)
    for unit in ("B", "K", "M", "G", "T"):
        if value < 1024 or unit == "T":
            return f"{value:.1f}{unit}"
        value /= 1024
    return f"{value:.1f}T"


def measure(candidates: list[Candidate], timeout_seconds: float) -> None:
    for candidate in candidates:
        if candidate.verdict == RECLAIMABLE:
            candidate.size_bytes = directory_size(candidate.path, timeout_seconds)


def report(candidates: list[Candidate], config: Policy, verbose: bool) -> None:
    for category in (WORKTREE_TARGET, DETACHED_TARGET, SHARED_INTERMEDIATE):
        rows = [candidate for candidate in candidates if candidate.category == category]
        reclaimable = [row for row in rows if row.verdict == RECLAIMABLE]
        total = sum(row.size_bytes or 0 for row in reclaimable)
        unknown = sum(1 for row in reclaimable if row.size_bytes is None)
        places = config.roots[category]
        where = f"{len(places)} root(s)" if places else "no root on this machine"
        print(
            f"{category}: {len(reclaimable)} of {len(rows)} reclaimable, {human(total)}"
            + (f" (+{unknown} unmeasured)" if unknown else "")
            + f"  [{where}, idle >= {config.idle_days[category]}d]"
        )
        if not verbose:
            continue
        for place in places:
            print(f"    root {place}")
        for row in sorted(rows, key=lambda row: -(row.size_bytes or 0)):
            note = row.detail or f"idle {row.idle_days:.1f}d"
            size = f" {human(row.size_bytes)}" if row.verdict == RECLAIMABLE else ""
            print(f"    {row.verdict:<22}{size:<10} {row.path}  ({note})")


def apply(candidates: list[Candidate]) -> int:
    freed = 0
    failures = 0
    for candidate in candidates:
        if candidate.verdict != RECLAIMABLE:
            continue
        try:
            shutil.rmtree(candidate.path)
        except OSError as error:
            print(f"could not remove {candidate.path}: {error}", file=sys.stderr)
            failures += 1
            continue
        freed += candidate.size_bytes or 0
        print(f"removed {candidate.path} ({human(candidate.size_bytes)})")
    print(f"reclaimed {human(freed)}")
    return 1 if failures else 0


def sccache_report(timeout_seconds: float = 180.0) -> list[str]:
    """The sccache disk cache's configured cap and current usage.

    It is capped in ``~/.config/sccache/config`` and therefore bounded rather
    than leaking, so this reports it and never reclaims it.
    """

    lines: list[str] = []
    config = Path(gate_tools.expand("$HOME/.config/sccache/config"))
    configured = None
    directory = None
    if config.is_file():
        for line in config.read_text(encoding="utf-8").splitlines():
            name, separator, value = line.partition("=")
            if not separator:
                continue
            if name.strip() == "size":
                try:
                    configured = int(value.strip())
                except ValueError:
                    configured = None
            elif name.strip() == "dir":
                directory = Path(value.strip().strip('"'))
    if configured is None and directory is None:
        return ["sccache: no disk cache configured"]
    usage = directory_size(directory, timeout_seconds) if directory and directory.is_dir() else None
    lines.append(
        f"sccache: capped at {human(configured)}, using {human(usage)}"
        + (f" in {directory}" if directory else "")
        + " (bounded by configuration; never reclaimed here)"
    )
    return lines


def headroom_report() -> list[str]:
    """Free space on each filesystem this repository's build output lands on."""

    config = policy()
    interesting = {Path(gate_tools.expand("$HOME")), gate_tools.target_directory()}
    for roots in config.roots.values():
        interesting.update(roots)
    seen: dict[tuple[int, int], str] = {}
    lines: list[str] = []
    for path in sorted(interesting):
        probe = path
        while not probe.exists() and probe != probe.parent:
            probe = probe.parent
        try:
            key = (os.stat(probe).st_dev, 0)
            usage = shutil.disk_usage(probe)
        except OSError:
            continue
        if key in seen:
            continue
        seen[key] = str(probe)
        lines.append(
            f"disk: {human(usage.free)} free of {human(usage.total)} "
            f"({usage.free * 100 // max(usage.total, 1)}%) at {probe}"
        )
    return lines


# ---------------------------------------------------------------- self-test


def _fixture(root: Path, now: float) -> tuple[Policy, set[Path], dict[str, Path]]:
    """One tree carrying every row of the decision table."""

    old = now - 30 * 86400
    young = now - 3600

    def touch(path: Path, when: float) -> None:
        path.mkdir(parents=True, exist_ok=True)
        (path / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
        for item in (path / "CACHEDIR.TAG", path):
            os.utime(item, (when, when))

    worktrees = root / "worktrees"
    detached = root / "targets"
    shared = root / "targets" / "cargo-intermediates"
    made: dict[str, Path] = {}

    for name, when in (("idle", old), ("young", young), ("busy", old)):
        checkout = worktrees / name
        (checkout / ".git").mkdir(parents=True)
        (checkout / "src").mkdir()
        (checkout / "src" / "lib.rs").write_text("// source\n")
        touch(checkout / "target", when)
        made[f"worktree-{name}"] = checkout / "target"
        made[f"checkout-{name}"] = checkout
    # A directory that is not a checkout at all: never scanned as one.
    (worktrees / "plain").mkdir()
    (worktrees / "plain" / "target").mkdir()
    made["plain-target"] = worktrees / "plain" / "target"

    for name, when in (("idle", old), ("young", young), ("building", old)):
        touch(detached / name, when)
        made[f"detached-{name}"] = detached / name
    # A checkout that wandered into the target root must be left alone — and it
    # has to be old and to look like cargo output, or the age and shape guards
    # would keep it and the checkout guard would never be the reason.
    checkout = detached / "a-checkout"
    (checkout / ".git").mkdir(parents=True)
    (checkout / "src").mkdir()
    (checkout / "src" / "lib.rs").write_text("// source\n")
    touch(checkout, old)
    (checkout / "debug").mkdir()
    for item in (
        checkout / "debug", checkout / ".git", checkout / "src" / "lib.rs",
        checkout / "src", checkout,
    ):
        os.utime(item, (old, old))
    made["detached-checkout"] = checkout
    # An old directory under the same root that is not build output at all.
    source_only = detached / "not-build-output"
    source_only.mkdir()
    (source_only / "notes.txt").write_text("keep me\n")
    for item in (source_only / "notes.txt", source_only):
        os.utime(item, (old, old))
    made["detached-source-only"] = source_only

    for name, when in (("aa", old), ("bb", young)):
        blob = shared / name
        blob.mkdir(parents=True)
        (blob / "object").write_bytes(b"x")
        os.utime(blob / "object", (when, when))
        os.utime(blob, (when, when))
        made[f"shared-{name}"] = blob

    config = Policy(
        idle_days={WORKTREE_TARGET: 7, DETACHED_TARGET: 7, SHARED_INTERMEDIATE: 3},
        roots={
            WORKTREE_TARGET: [worktrees],
            DETACHED_TARGET: [detached],
            SHARED_INTERMEDIATE: [shared],
        },
        safety_rules=[],
        needs_a_decision=[],
    )
    live = {made["checkout-busy"], made["detached-building"]}
    return config, live, made


def self_test() -> int:
    """Prove the decision table, and prove the policy is readable from the manifest.

    | Rule | cause                                  | effect                 |
    |------|----------------------------------------|------------------------|
    | D1   | a checkout is in use                   | its target is kept     |
    | D2   | a target was built within the threshold| kept                   |
    | D3   | an old, idle target                    | reclaimable            |
    | D4   | a detached target a process builds into| kept                   |
    | D5   | a checkout or a .git path              | never a candidate      |
    | D6   | an old shared intermediates entry      | reclaimable            |
    | D7   | a young shared intermediates entry     | kept                   |
    | D8   | apply removes only the reclaimable     | sources survive intact |
    | D9   | the manifest's own policy              | parses with real roots |
    """

    problems: list[str] = []
    now = time.time()
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        config, live, made = _fixture(root, now)
        found = {candidate.path: candidate for candidate in scan(config, live, now)}

        expected = [
            ("D1", made["worktree-busy"], KEPT_IN_USE),
            ("D2", made["worktree-young"], KEPT_YOUNG),
            ("D3", made["worktree-idle"], RECLAIMABLE),
            ("D4", made["detached-building"], KEPT_IN_USE),
            ("D3", made["detached-idle"], RECLAIMABLE),
            ("D2", made["detached-young"], KEPT_YOUNG),
            ("D6", made["shared-aa"], RECLAIMABLE),
            ("D7", made["shared-bb"], KEPT_YOUNG),
        ]
        for rule, path, verdict in expected:
            actual = found.get(path)
            if actual is None:
                problems.append(f"{rule} {path} was never considered")
            elif actual.verdict != verdict:
                problems.append(f"{rule} {path} was {actual.verdict}, expected {verdict}")

        # D5 is the rule that matters most, so it is asserted positively: the
        # old checkout below is scanned, is older than the threshold, and does
        # look like cargo output, so only the checkout guard can keep it.
        for rule, path, verdict in (
            ("D5", made["detached-checkout"], KEPT_PROTECTED),
            ("D5", made["detached-source-only"], KEPT_PROTECTED),
        ):
            candidate = found.get(path)
            if candidate is None:
                problems.append(f"{rule} {path} was never considered, so nothing protected it")
            elif candidate.verdict != verdict:
                problems.append(
                    f"{rule} {path} was {candidate.verdict}, expected {verdict}"
                )
        for rule, path in (("D5", made["checkout-idle"]), ("D5", made["plain-target"])):
            candidate = found.get(path)
            if candidate is not None and candidate.verdict == RECLAIMABLE:
                problems.append(f"{rule} {path} must never be reclaimable")

        reclaimable = [c for c in found.values() if c.verdict == RECLAIMABLE]
        import contextlib
        import io

        with contextlib.redirect_stdout(io.StringIO()):
            apply(reclaimable)
        for path in (made["worktree-idle"], made["detached-idle"], made["shared-aa"]):
            if path.exists():
                problems.append(f"D8 {path} should have been removed")
        for path in (
            made["checkout-idle"],
            made["checkout-idle"] / ".git",
            made["checkout-idle"] / "src" / "lib.rs",
            made["worktree-young"],
            made["worktree-busy"],
            made["detached-checkout"],
            made["shared-bb"],
        ):
            if not path.exists():
                problems.append(f"D8 {path} must have survived")

    try:
        real = policy()
        if not real.safety_rules or not real.needs_a_decision:
            problems.append("D9 the manifest states no safety rules")
        if any(days <= 0 for days in real.idle_days.values()):
            problems.append("D9 an idle threshold is not positive")
    except (KeyError, ValueError) as error:
        problems.append(f"D9 the manifest policy is unreadable: {error}")

    if problems:
        print("reclaim_build_space self-test:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(
        "reclaim_build_space self-test: in-use kept, young kept, old and idle reclaimed, "
        "checkouts never touched"
    )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="remove the reclaimable paths")
    parser.add_argument("--dry-run", action="store_true", help="the default; report only")
    parser.add_argument("--summary", action="store_true", help="one line per category")
    parser.add_argument("--verbose", action="store_true", help="one line per path")
    parser.add_argument("--size-timeout-seconds", type=float, default=30.0)
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args(argv)

    if arguments.self_test:
        return self_test()
    if arguments.apply and arguments.dry_run:
        print("--apply and --dry-run are opposites; choose one", file=sys.stderr)
        return 2

    config = policy()
    candidates = scan(config)
    measure(candidates, arguments.size_timeout_seconds)
    for line in headroom_report():
        print(line)
    report(candidates, config, verbose=arguments.verbose and not arguments.summary)
    for line in sccache_report(max(arguments.size_timeout_seconds, 180.0)):
        print(line)
    if arguments.summary:
        return 0
    if not arguments.apply:
        print("")
        print("dry run: nothing was removed. Re-run with --apply to reclaim the paths above.")
        print("safety rules (scripts/ci/gate_tools.json, docs/adr/0080):")
        for rule in config.safety_rules:
            print(f"  - {rule}")
        print("not this tool's business:")
        for item in config.needs_a_decision:
            print(f"  - {item}")
        return 0
    return apply(candidates)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
