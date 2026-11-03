#!/usr/bin/env python3
"""Refuse a gate entry point that accepts an argument it does not implement.

`scripts/ci/check-all.sh` invoked `check_degrading_defaults.py --selftest` while
that script's flag was `--self-test`. The script tested `"--selftest" in
sys.argv`, found nothing, fell through to its ordinary whole-repo scan and
exited zero. So the gate was green, the caller believed a self-test had run, and
that self-test had in fact never executed. Nothing failed, because nothing
refused.

The shape is not one script's bug. A checker that decides what to do by asking
whether one spelling is present has no way to say "I do not implement that", and
a shell entry point that matches `"$1"` against one literal and continues has
the same hole. The rule therefore runs over the *set* of gate entry points, so a
checker added tomorrow inherits it:

  A1  every entry point, given a flag it does not implement, exits non-zero and
      prints a `usage:` line naming itself. Falling through to the default work
      is refused; so is exiting zero; so is running long enough to hit the
      probe deadline, because "it did not refuse" is the defect either way.
  A2  one canonical spelling per flag. argparse accepts unambiguous prefixes
      unless `allow_abbrev=False`, so `--self-tes` was a second spelling of
      every flag in this repository. The construction is checked statically over
      every parser, and the behaviour dynamically at one truncation per entry
      point.
  A3  a lane may only pass a flag its target declares. A1 turns a misspelled
      call site into a red gate at run time; A3 names it in seconds, statically,
      which is where `--selftest` should have been caught.
  A4  a refusal leaves no trace. Probing the corpus once wrote a 1.7 MB
      `awaken-sandbox` build to `./--awaken-gate-argument-probe`, and the next
      `git add -A` committed it: an entry point had used the flag as an output
      path before exiting non-zero, so A1 saw a refusal and nothing looked at
      the working tree. Every path the probes create or rewrite is now an error
      naming the entry point that wrote it.

The declared flag set is read back from the entry point's own refusal output, so
this file holds no second copy of anybody's argument list to go stale.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import formal_sources

from _native_shell import bash_executable, self_test as native_shell_self_test

# Every file this checker reads: the entry points it probes and the lanes whose
# call sites it compares against them. The lefthook trigger is rendered from
# this declaration, so a checker added under scripts/ci re-runs the corpus rule
# at the commit that introduces it.
INPUT_GLOBS: tuple[str, ...] = (
    "lefthook.yml",
    "scripts/ci/*.py",
    "scripts/ci/*.sh",
    "scripts/ci/pre-commit/observe.sh",
    "scripts/contract/*.sh",
    "scripts/contract/*.py",
    "scripts/e2e/*.sh",
    "scripts/release/*.py",
    "scripts/release/*.sh",
    "e2e/*.sh",
    "e2e/*.py",
    "e2e/k3d/*.py",
    "e2e/k3d/*.sh",
    "e2e/k6/*.sh",
    "deploy/images/sandbox/build.sh",
    "deploy/k3d/test_image_contract.py",
    "CONTRIBUTING.md",
)

ROOT = Path(__file__).resolve().parents[2]

# A flag no entry point implements. It is not `--help`: `--help` is implemented,
# and an entry point that handles it proves nothing about what it refuses.
PROBE_FLAG = "--awaken-gate-argument-probe"
# Refusing an unimplemented flag is a parse-time decision and takes
# milliseconds. This deadline is generous by two orders of magnitude on
# purpose: anything that reaches it has started doing work, which is the defect.
PROBE_TIMEOUT_SECONDS = 60

# Directories whose executables are gate entry points. A file is an entry point
# when it can be executed on its own: a `__main__` guard for Python, a main
# guard or a plain top-level body for shell. Container payload scripts
# (deploy/images/**) are excluded: they are invoked by a Dockerfile with a fixed
# argument vector and are not reachable from any lane on this side of the image.
ENTRY_POINT_DIRECTORIES = (
    "scripts/ci",
    "scripts/e2e",
    "scripts/contract",
    "scripts/release",
    "e2e",
    "e2e/k3d",
    "e2e/k6",
)

# Entry points a lane invokes from outside those directories. Listed by path so
# that adding one is a decision rather than a side effect of a glob.
ADDITIONAL_ENTRY_POINTS = (
    "scripts/ci/pre-commit/observe.sh",
    "deploy/images/sandbox/build.sh",
    "deploy/k3d/test_image_contract.py",
)

# Lanes: the files that select what runs. Every flag they pass to a corpus entry
# point is checked against that entry point's declared set (A3).
LANE_FILES = (
    "scripts/ci/check-all.sh",
    "scripts/ci/check-rust.sh",
    "scripts/ci/check-reliability.sh",
    "scripts/ci/check_formal.sh",
    "scripts/ci/check-docs.sh",
    "lefthook.yml",
    # The contributor guide is a lane for people: a command it tells someone to
    # run must name a script that exists and flags that script declares.
    "CONTRIBUTING.md",
)

# Key: repository-relative path. Value: why this executable is not subject to
# A1/A2. Only two reasons are admissible and both are structural.
ENTRY_POINT_EXEMPTIONS: dict[str, str] = {
    "scripts/ci/kani.sh": (
        "A transparent argument passthrough: it ends in `exec cargo kani \"$@\"`, "
        "so the argument vocabulary is Kani's and Kani refuses its own unknown "
        "flags. A usage line here would have to restate a vocabulary this "
        "repository does not own."
    ),
    "e2e/k3d/distributed_control_driver.ts": (
        "Driven by `pnpm exec tsx` from distributed_control_e2e.sh with a fixed "
        "argument vector; it is not independently invocable and has no lane."
    ),
}

# Shell files that are sourced into another script rather than executed. They
# have no argument vector of their own to admit or refuse.
SOURCED_LIBRARY = re.compile(r"^_|/_")

USAGE_LINE = re.compile(r"^\s*usage:", re.IGNORECASE | re.MULTILINE)
LONG_FLAG = re.compile(r"--[a-z][a-z0-9]*(?:-[a-z0-9]+)*")
PARSER_CONSTRUCTION = "argparse.ArgumentParser("
# A lane invocation: an optional `python3`/`bash` prefix, a repository path, and
# whatever flags follow on the same logical line.
LANE_INVOCATION = re.compile(
    r"(?:python3|bash|sh|env\s+\S+)?\s*((?:scripts|e2e|deploy)/[\w./-]+\.(?:py|sh))((?:\s+[^\s|&;<>]+)*)"
)


class Probe:
    """What one entry point did when handed a flag it does not implement."""

    def __init__(self, relative: str, status: int, output: str) -> None:
        self.relative = relative
        self.status = status
        self.output = output

    @property
    def refused(self) -> bool:
        return self.status != 0

    @property
    def named_itself(self) -> bool:
        name = Path(self.relative).name
        return any(name in line for line in self.output.splitlines() if USAGE_LINE.match(line))

    @property
    def declared_flags(self) -> set[str]:
        """Flags the entry point names in its own refusal output.

        Read back rather than restated here, so this file cannot hold a stale
        second copy of anybody's argument list.
        """

        found = set(LONG_FLAG.findall(self.output))
        return {flag for flag in found if flag != PROBE_FLAG} - {"--help"}


def _entry_point_filename(path: Path) -> bool:
    """The original filename exclusions, before any source acquisition."""
    return (not path.name.startswith(".") and not SOURCED_LIBRARY.search("/" + path.name)
            and path.suffix in (".py", ".sh"))


def is_entry_point(path: Path, text: str) -> bool:
    """Classify the caller's selected text without reading a second source."""
    if not _entry_point_filename(path):
        return False
    if path.suffix == ".py":
        return '__main__' in text
    if path.suffix == ".sh":
        return text.startswith("#!")
    return False


def corpus(root: Path) -> list[str]:
    """Every gate entry point, in a stable order."""

    found: list[str] = []
    for directory in ENTRY_POINT_DIRECTORIES:
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.iterdir()):
            if not path.is_file() or not _entry_point_filename(path):
                continue
            if not is_entry_point(path, path.read_text(encoding="utf-8", errors="ignore")):
                continue
            relative = path.relative_to(root).as_posix()
            if relative in ENTRY_POINT_EXEMPTIONS or relative in found:
                continue
            found.append(relative)
    for relative in ADDITIONAL_ENTRY_POINTS:
        if (root / relative).is_file() and relative not in ENTRY_POINT_EXEMPTIONS:
            found.append(relative)
    return found


def probe(root: Path, relative: str, flag: str = PROBE_FLAG) -> Probe:
    path = root / relative
    command = [sys.executable, str(path)] if path.suffix == ".py" else [bash_executable(), path.as_posix()]
    # The shared owner closes every captured descendant pipe on timeout and
    # normal leader exit. subprocess.run(timeout=...) kills only the immediate
    # Windows child and can wait forever for an orphan's inherited pipe.
    result = subprocess.run(
        [sys.executable, str(ROOT / "scripts/ci/deadline.py"), str(PROBE_TIMEOUT_SECONDS), "--", *command, flag],
        cwd=root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    # A deadline is a refusal failure even if the hanging command printed a
    # usage line first. A timed-out probe must never count as admission.
    return Probe(relative, 0 if result.returncode == 124 else result.returncode, result.stdout + result.stderr)


Footprint = dict[str, tuple[int, int]]


def working_tree_footprint(root: Path) -> Footprint:
    """Every untracked-and-unignored or modified path, with its mtime and size.

    Ignored paths (bytecode caches, build output) are outside the rule: writing
    them is not a trace a commit can pick up.
    """

    # A temporary tree inside target/ is not the parent checkout. Git would
    # succeed there while hiding the fixture through the parent's ignore rules.
    # Match the other source guards: only an exact repository top owns an index;
    # malformed repositories fail, and ordinary fixtures are walked on disk.
    if (root / ".git").exists():
        names = formal_sources.git_paths(
            root, "ls-files", "-z", "--others", "--modified", "--exclude-standard",
        )
    else:
        names = [
            path.relative_to(root).as_posix()
            for path in root.rglob("*")
            if path.is_file() and "__pycache__" not in path.relative_to(root).parts
        ]
    footprint: Footprint = {}
    for name in names:
        try:
            stat = (root / name).stat()
        except FileNotFoundError:
            continue
        footprint[name] = (stat.st_mtime_ns, stat.st_size)
    return footprint


def traces(before: Footprint, after: Footprint) -> list[str]:
    return sorted(name for name, identity in after.items() if before.get(name) != identity)


def trace_errors(root: Path, before: Footprint, probed: list[tuple[str, str]]) -> list[str]:
    """A4: attribute every new or rewritten path to the entry point that wrote it.

    The corpus is probed in parallel, so one footprint after the whole batch
    only says that *something* wrote. Attribution re-probes sequentially and
    compares the footprint around each call; a path the first pass created is
    rewritten by the second, which changes its mtime.
    """

    left = traces(before, working_tree_footprint(root))
    if not left:
        return []
    errors: list[str] = []
    attributed: set[str] = set()
    for relative, flag in probed:
        ahead = working_tree_footprint(root)
        probe(root, relative, flag)
        written = traces(ahead, working_tree_footprint(root))
        if written:
            attributed.update(written)
            errors.append(
                f"{relative}: given {flag}, which it does not implement, it wrote "
                f"{', '.join(written)} before refusing; an unimplemented flag must be refused "
                "before any effect, or the flag becomes a path a later `git add` commits"
            )
    unattributed = [name for name in left if name not in attributed]
    if unattributed:
        errors.append(
            f"the gate-argument probes left {', '.join(unattributed)} in the working tree and no "
            "single entry point reproduced it when re-probed alone; remove it and probe again"
        )
    return errors


def probe_all(root: Path, relatives: list[str]) -> dict[str, Probe]:
    with ThreadPoolExecutor(max_workers=12) as pool:
        results = list(pool.map(lambda relative: probe(root, relative), relatives))
    return {result.relative: result for result in results}


def refusal_errors(probes: dict[str, Probe]) -> list[str]:
    """A1: an unimplemented flag is refused by name."""

    errors: list[str] = []
    for relative, result in sorted(probes.items()):
        if not result.refused:
            errors.append(
                f"{relative}: exits {result.status} when given {PROBE_FLAG}, an argument it does "
                "not implement; it ran its default work instead of refusing, so a misspelled "
                "flag at a call site is silently ignored. Parse arguments strictly "
                "(argparse with allow_abbrev=False, or a shell case whose default arm exits)."
            )
        elif not result.named_itself:
            errors.append(
                f"{relative}: refuses {PROBE_FLAG} but prints no `usage:` line naming itself, so "
                "the caller learns that something was wrong and not which spellings exist"
            )
    return errors


def parser_constructions(text: str) -> list[str]:
    """The argument text of every ``argparse.ArgumentParser(...)`` call.

    Read to the parenthesis that closes the call, not the first one, so a call
    such as ``ArgumentParser(prog=Path(__file__).name, allow_abbrev=False)`` is
    read whole.
    """
    found: list[str] = []
    start = text.find(PARSER_CONSTRUCTION)
    while start >= 0:
        opening = start + len(PARSER_CONSTRUCTION)
        depth, cursor = 1, opening
        while cursor < len(text) and depth:
            depth += {"(": 1, ")": -1}.get(text[cursor], 0)
            cursor += 1
        found.append(text[opening:cursor - 1])
        start = text.find(PARSER_CONSTRUCTION, cursor)
    return found


def abbreviation_construction_errors(sources: dict[str, str]) -> list[str]:
    """A2, statically: every parser states that a flag has one spelling."""

    errors: list[str] = []
    for relative, text in sorted(sources.items()):
        for arguments in parser_constructions(text):
            if "allow_abbrev" not in arguments:
                errors.append(
                    f"{relative}: an argparse parser is constructed without allow_abbrev=False, "
                    "so every unambiguous prefix of every flag is a second accepted spelling of "
                    "it and a truncated flag cannot be told from the intended one"
                )
    return errors


def abbreviation_truncations(probes: dict[str, Probe]) -> list[tuple[str, str]]:
    truncations: list[tuple[str, str]] = []
    for relative, result in sorted(probes.items()):
        declared = result.declared_flags
        if not declared:
            continue
        longest = max(declared, key=len)
        if len(longest) < 6:
            continue
        truncated = longest[:-1]
        if truncated in declared:
            continue
        truncations.append((relative, truncated))
    return truncations


def abbreviation_behaviour_errors(root: Path, probes: dict[str, Probe]) -> list[str]:
    """A2, dynamically: one truncation per entry point is actually refused."""

    truncations = abbreviation_truncations(probes)
    with ThreadPoolExecutor(max_workers=12) as pool:
        outcomes = list(pool.map(lambda pair: probe(root, pair[0], pair[1]), truncations))
    errors = []
    for (relative, truncated), outcome in zip(truncations, outcomes):
        if not outcome.refused:
            errors.append(
                f"{relative}: accepts {truncated}, an abbreviation of a declared flag, so that "
                "flag has more than one spelling and a typo is indistinguishable from intent"
            )
    return errors


def lane_call_site_errors(lanes: dict[str, str], declared: dict[str, set[str]], root: Path) -> list[str]:
    """A3: a lane may only invoke a path that exists, with flags its target declares."""

    errors: list[str] = []
    for lane, text in sorted(lanes.items()):
        if lane == 'lefthook.yml':
            from generate_hooks import pre_commit_view
            text = pre_commit_view(text)
        for line in text.splitlines():
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            for match in LANE_INVOCATION.finditer(stripped):
                target = match.group(1)
                if not (root / target).exists():
                    errors.append(f"{lane}: invokes {target}, which does not exist")
                    continue
                if target not in declared:
                    continue
                for flag in LONG_FLAG.findall(match.group(2)):
                    if flag in declared[target]:
                        continue
                    known = ", ".join(sorted(declared[target])) or "(none)"
                    errors.append(
                        f"{lane}: invokes {target} {flag}, which {target} does not declare; "
                        f"it declares {known}"
                    )
    return errors


def python_sources(root: Path) -> dict[str, str]:
    """Every Python file in the corpus directories, entry point or helper.

    The spelling rule is over parser *construction*, not over entry points: a
    helper module that builds a parser hands its abbreviations to whatever
    imports it.
    """

    sources = {}
    for directory in ENTRY_POINT_DIRECTORIES:
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.glob("*.py")):
            sources[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    for relative in ADDITIONAL_ENTRY_POINTS:
        path = root / relative
        if path.suffix == ".py" and path.is_file():
            sources[relative] = path.read_text(encoding="utf-8")
    return sources


def validate(root: Path) -> list[str]:
    relatives = corpus(root)
    if len(relatives) < 30:
        return [
            f"gate argument corpus collapsed to {len(relatives)} entry points; a rule over the "
            "set is worthless when the set is empty, so discovery failing is itself a failure"
        ]
    before = working_tree_footprint(root)
    probes = probe_all(root, relatives)
    declared = {relative: result.declared_flags for relative, result in probes.items()}
    lanes = {
        relative: (root / relative).read_text(encoding="utf-8")
        for relative in LANE_FILES
        if (root / relative).is_file()
    }
    errors = refusal_errors(probes)
    errors += abbreviation_construction_errors(python_sources(root))
    errors += abbreviation_behaviour_errors(root, probes)
    errors += lane_call_site_errors(lanes, declared, root)
    probed = [(relative, PROBE_FLAG) for relative in relatives] + abbreviation_truncations(probes)
    errors += trace_errors(root, before, probed)
    for relative, reason in sorted(ENTRY_POINT_EXEMPTIONS.items()):
        if not (root / relative).exists():
            errors.append(
                f"{relative}: ENTRY_POINT_EXEMPTIONS names a file that no longer exists; drop the "
                f"stale exemption ({reason.split('.')[0]})"
            )
    return errors


def self_test() -> None:
    """Decision table, one row per shape the survey of this repository found.

    Causes, over an entry point given a flag it does not implement:
      C1 it exits non-zero and prints a `usage:` line naming itself
      C2 it exits zero, having fallen through to its default work
      C3 it exits non-zero but prints no usage line
      C4 it never returns within the probe deadline
      C5 it is a declared exemption (an argument passthrough)
    Causes, over spelling:
      C6 an argparse parser passes allow_abbrev=False
      C7 an argparse parser omits it
      C8 a truncated declared flag is refused
      C9 a truncated declared flag is accepted
    Causes, over lanes:
      C10 a lane passes a flag the target declares
      C11 a lane passes a flag the target does not declare
      C12 a lane passes no flag at all
      C15 a lane invokes a path that does not exist
    Causes, over the working tree:
      C13 a refusal writes nothing
      C14 a refusal first writes the flag as a path
    Effects: E1 admitted, E2 rejected by name.
    Rules: A1 = C1|C5 => E1; C2|C3|C4 => E2.
           A2 = C6|C8 => E1; C7|C9 => E2.
           A3 = C10|C12 => E1; C11|C15 => E2.
           A4 = C13 => E1; C14 => E2 naming the writer.
    """

    import tempfile  # noqa: PLC0415 - only the self-test writes fixtures

    native_shell_self_test()

    # C16 cause/effect: caller-selected text and original filename exclusions
    # determine entry membership. Python main substring / shell shebang admit;
    # helpers, hidden/sourced names and other suffixes refuse. The predicate is
    # pure: even a selected path whose reader raises must not perform IO. The
    # caller still owns the original ignore-UTF8 acquisition in corpus().
    from unittest.mock import patch

    with patch.object(Path, "open", side_effect=AssertionError("predicate must not read")):
        for name, text, expected in (
            ("check_one.py", 'if __name__ == "__main__": pass', True),
            ("check_one.py", '# __main__ is still the original substring predicate', True),
            ("check_one.py", "pass", False),
            ("one.sh", "#!/usr/bin/env bash\n", True),
            ("one.sh", "echo no shebang", False),
            (".hidden.py", "__main__", False),
            ("_sourced.py", "__main__", False),
            ("helper.txt", "__main__", False),
        ):
            assert is_entry_point(Path(name), text) is expected, "C16 original pure entry predicate"

    # C17 graph: eligible filename -> one ignore-decoded source read -> original
    # body predicate -> membership. An excluded filename stops before reading,
    # even if its reader would fail; evaluating read_text before is_entry_point
    # reversed that precondition and made hidden/sourced files fail discovery.
    #
    # | Row | filename             | body                   | effect |
    # | C17a| hidden/sourced py/sh  | reader refuses         | zero reads, excluded |
    # | C17b| ordinary py/sh        | main substring/shebang | one read, included |
    # | C17c| ordinary py/sh        | neither                | one read, excluded |
    # | C17d| another suffix        | reader refuses         | zero reads, excluded |
    # Constraint: corpus(), not a stub predicate, must enforce the pre-read
    # qualification; the same owner predicate also serves is_entry_point().
    from collections import Counter

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        directory = root / "scripts/ci"
        directory.mkdir(parents=True)
        excluded = {".hidden.py", ".hidden.sh", "_sourced.py", "_sourced.sh", "other.txt"}
        for name in excluded:
            (directory / name).write_text("__main__", encoding="utf-8")
        for name, source in (("plain.py", "__main__"), ("plain.sh", "#!/bin/sh\n"),
                             ("helper.py", "pass"), ("no_header.sh", "echo no shebang")):
            (directory / name).write_text(source, encoding="utf-8")
        reads = Counter()
        original_open = Path.open

        def opened(path, *args, **kwargs):
            if path.parent == directory:
                reads[path.name] += 1
                if path.name in excluded:
                    raise AssertionError("excluded filename must not be read")
            return original_open(path, *args, **kwargs)

        with patch.object(Path, "open", opened):
            found = corpus(root)
        assert found == ["scripts/ci/plain.py", "scripts/ci/plain.sh"], "C17 original membership"
        assert reads == {"plain.py": 1, "plain.sh": 1, "helper.py": 1, "no_header.sh": 1}, \
            "C17 real corpus reads ordinary files once and excluded files never"

    strict = (
        "#!/usr/bin/env bash\n"
        'case "${1:-}" in\n  ""|--self-test) ;;\n'
        '  *) echo "usage: strict.sh [--self-test]" >&2; exit 2 ;;\nesac\n'
        'echo ran\n'
    )
    falls_through = '#!/usr/bin/env bash\nif [ "${1:-}" = "--self-test" ]; then exit 0; fi\necho ran\n'
    silent_refusal = '#!/usr/bin/env bash\nexit 3\n'
    never_returns = "#!/usr/bin/env bash\nsleep 600 &\nwait\n"
    usage_then_hangs = "#!/usr/bin/env bash\necho 'usage: usage_then_hangs.sh [--self-test]'\nsleep 600 &\nwait\n"

    with tempfile.TemporaryDirectory() as temporary:
        # C18: exercise the actual process/trace table inside an ignored child
        # of another repository, as native Windows CI's short TMP does.
        parent = Path(temporary)
        subprocess.run(["git", "init", "--quiet", "--template=", str(parent)],
                       check=True, env=formal_sources.detached_git_environment())
        (parent / ".gitignore").write_text("target/\n", encoding="utf-8")
        root = parent / "target" / "fixture"
        directory = root / "scripts/ci"
        directory.mkdir(parents=True)
        for name, body in (
            ("strict.sh", strict),
            ("falls_through.sh", falls_through),
            ("silent_refusal.sh", silent_refusal),
        ):
            (directory / name).write_text(body, encoding="utf-8")
        probes = probe_all(root, ["scripts/ci/strict.sh", "scripts/ci/falls_through.sh", "scripts/ci/silent_refusal.sh"])
        errors = refusal_errors(probes)
        assert not [e for e in errors if "strict.sh" in e], "C1/E1"
        assert [e for e in errors if "falls_through.sh" in e], "C2/E2"
        assert [e for e in errors if "silent_refusal.sh" in e], "C3/E2"

        # A tool can diagnose an OS error in a different encoding. Preserve its
        # failure for A1 instead of crashing a reader thread and losing stdout.
        encoded = directory / "encoded.py"
        encoded.write_text(
            "import sys\nsys.stdout.buffer.write(bytes([224, 255]))\nraise SystemExit(3)\n",
            encoding="utf-8",
        )
        observed = probe(root, "scripts/ci/encoded.py")
        assert observed.status == 3 and "\ufffd" in observed.output
        assert refusal_errors({observed.relative: observed}), "C3/E2 malformed diagnostic is not admission"

        # C4: the deadline is a refusal failure, not an inconclusive probe.
        (directory / "never_returns.sh").write_text(never_returns, encoding="utf-8")
        (directory / "usage_then_hangs.sh").write_text(usage_then_hangs, encoding="utf-8")
        global PROBE_TIMEOUT_SECONDS
        original_timeout = PROBE_TIMEOUT_SECONDS
        PROBE_TIMEOUT_SECONDS = 1
        try:
            slow = probe_all(root, ["scripts/ci/never_returns.sh", "scripts/ci/usage_then_hangs.sh"])
            assert refusal_errors(slow), "C4/E2 a probe deadline is a refusal failure"
            assert all(result.status == 0 for result in slow.values()), "C4/E2 usage before a hang is not refusal"
            assert "usage:" in slow["scripts/ci/usage_then_hangs.sh"].output, "C4 captured output survives tree cleanup"
        finally:
            PROBE_TIMEOUT_SECONDS = original_timeout

        # C5: an exemption removes a file from the corpus, and a stale one is
        # itself an error.
        assert "scripts/ci/strict.sh" in corpus(root), "C5 baseline"
        ENTRY_POINT_EXEMPTIONS["scripts/ci/strict.sh"] = "fixture"
        try:
            assert "scripts/ci/strict.sh" not in corpus(root), "C5/E1"
        finally:
            del ENTRY_POINT_EXEMPTIONS["scripts/ci/strict.sh"]

        # C6/C7: the spelling rule over parser construction.
        # Assembled rather than written as literals: this checker scans its own
        # directory, and a fixture spelled out here would be found by the rule
        # it is a fixture for.
        construction = "argparse.ArgumentParser("
        assert not abbreviation_construction_errors(
            {"a.py": construction + "allow_abbrev=False, description=__doc__)"}
        ), "C6/E1"
        assert abbreviation_construction_errors(
            {"a.py": construction + "description=__doc__)"}
        ), "C7/E2"
        assert not abbreviation_construction_errors(
            {"a.py": construction + "prog=Path(__file__).name, allow_abbrev=False)"}
        ), "C6/E1 a nested call is read to the closing parenthesis"
        assert abbreviation_construction_errors(
            {"a.py": construction + "prog=Path(__file__).name)\nx = f(allow_abbrev=False)"}
        ), "C7/E2 a later call's argument is not the parser's"

        # C8/C9: the spelling rule as behaviour. `strict.sh` declares
        # --self-test and refuses --self-tes; the abbreviating fixture accepts it.
        abbreviating = (
            "#!/usr/bin/env bash\n"
            'case "${1:-}" in\n  --self-tes*) exit 0 ;;\n'
            '  *) echo "usage: abbreviating.sh [--self-test]" >&2; exit 2 ;;\nesac\n'
        )
        (directory / "abbreviating.sh").write_text(abbreviating, encoding="utf-8")
        spelling = probe_all(root, ["scripts/ci/strict.sh", "scripts/ci/abbreviating.sh"])
        behaviour = abbreviation_behaviour_errors(root, spelling)
        assert not [e for e in behaviour if "strict.sh" in e], "C8/E1"
        assert [e for e in behaviour if "abbreviating.sh" in e], "C9/E2"

        # C10/C11/C12: the lane rule, including the exact defect this file was
        # written for -- a lane that spells the flag its target does not have.
        declared = {"scripts/ci/strict.sh": {"--self-test"}}
        assert not lane_call_site_errors(
            {"lane.sh": "run static x scripts/ci/strict.sh --self-test\n"}, declared, root
        ), "C10/E1"
        assert lane_call_site_errors(
            {"lane.sh": "run static x scripts/ci/strict.sh --selftest\n"}, declared, root
        ), "C11/E2 the reported defect"
        assert not lane_call_site_errors(
            {"lane.sh": "run static x scripts/ci/strict.sh\n"}, declared, root
        ), "C12/E1"
        assert not lane_call_site_errors(
            {"lane.sh": "# scripts/ci/strict.sh --selftest\n"}, declared, root
        ), "C12/E1 a comment is documentation, not a call site"
        assert lane_call_site_errors(
            {"guide.md": "    scripts/ci/gone.sh --self-test\n"}, declared, root
        ) == ["guide.md: invokes scripts/ci/gone.sh, which does not exist"], "C15/E2"

        # C13/C14: the committed-binary defect. `writes_flag.sh` treats its
        # argument as an output path and then refuses; A1 alone admits it.
        writes_flag = (
            "#!/usr/bin/env bash\n"
            'cd "$(dirname "$0")/../.."\n'
            'printf binary > "./$1"\n'
            'echo "usage: writes_flag.sh" >&2; exit 2\n'
        )
        (directory / "writes_flag.sh").write_text(writes_flag, encoding="utf-8")
        before = working_tree_footprint(root)
        pair = probe_all(root, ["scripts/ci/strict.sh", "scripts/ci/writes_flag.sh"])
        assert not refusal_errors(pair), "C14 is invisible to A1: that is the defect"
        found = trace_errors(
            root,
            before,
            [("scripts/ci/strict.sh", PROBE_FLAG), ("scripts/ci/writes_flag.sh", PROBE_FLAG)],
        )
        assert [e for e in found if e.startswith("scripts/ci/writes_flag.sh:")], "C14/E2 names the writer"
        assert not [e for e in found if e.startswith("scripts/ci/strict.sh:")], "C13/E1"
        (root / PROBE_FLAG).unlink()
        clean = working_tree_footprint(root)
        assert not trace_errors(root, clean, [("scripts/ci/strict.sh", PROBE_FLAG)]), "C13/E1 alone"

    print("check_gate_arguments self-test: the decision table holds")


def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_gate_arguments.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the corpus"
    )
    return parser.parse_args(argv)


if __name__ == "__main__":
    if parse(sys.argv[1:]).self_test:
        self_test()
        raise SystemExit(0)
    problems = validate(ROOT)
    for problem in problems:
        print(f"ERROR: {problem}", file=sys.stderr)
    if problems:
        raise SystemExit(1)
    print(
        f"OK - {len(corpus(ROOT))} gate entry points refuse an argument they do not implement, "
        "each flag has one spelling, and every lane spells its call sites as declared."
    )
