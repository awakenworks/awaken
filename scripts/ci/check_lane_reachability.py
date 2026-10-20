#!/usr/bin/env python3
"""Refuse an executable that no lane runs, and evidence that cites one.

Three k3d end-to-end runners sat in `e2e/k3d/` for months with the executable
bit set, a cleanup trap, and assertions about cross-node failover — and no lane
invoked any of them. One of them, `distributed_control_contract_test.sh`, is
named in the formal obligation ledger as the evidence for a proved requirement. So a
proof ledger cited a script that had not run, and nothing said so: a gate that
passes because it never observed its subject.

An unrun script is not the same as a deleted one. It still reads like coverage
to anyone browsing the tree, it still appears in evidence lists, and it rots in
place until the day someone runs it and discovers it stopped compiling two
refactors ago. The answer is not to delete them and not to pretend: every
executable is either *selected* — reachable from a lane — or *registered*, with
the reason written down and the coverage it is not providing named.

  L1  every executable under e2e/**, scripts/** and deploy/** is reachable from
      a lane root, or appears in UNSELECTED_EXECUTABLES with a reason.
  L2  every evidence path in the obligation ledger that is an executable must be
      selected by a lane. A registry entry cannot excuse this one: the ledger
      claims the script is what makes a requirement true.
  L3  a registry entry that names a file which is now selected, or which no
      longer exists, is stale and fails. The registry is the set of things CI is
      not covering; an entry nobody can defend must not survive by inertia.

Reachability is computed, not declared. From each lane root the closure follows
every repository path the selected files mention outside a comment, so wiring a
script into an existing runner is enough to select it and no second list has to
be updated.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

import formal_ledger
import formal_sources

# Every file this checker reads: the lane roots whose closure it walks, the
# executables it accounts for, and the obligation ledger (its TOML files and
# the Rust anchors and Cargo manifests it is assembled from).
INPUT_GLOBS: tuple[str, ...] = (
    "lefthook.yml",
    "formal/coverage.toml",
    "formal/obligations/**/*",
    "web/src/**/*",
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "scripts/**/*",
    "e2e/**/*",
    "deploy/**/*",
)

ROOT = Path(__file__).resolve().parents[2]

# Every file that decides what runs. A file is selected when the closure from
# one of these reaches it.
LANE_ROOTS = (
    "scripts/ci/check-all.sh",
    "lefthook.yml",
    "e2e/package.json",
    "package.json",
)
WORKFLOW_GLOB = ".github/workflows/*.yml"

# Directories whose executables must be accounted for.
SUBJECT_DIRECTORIES = ("e2e", "scripts", "deploy")
SKIPPED_PARTS = frozenset({"node_modules", "__pycache__", "target", ".git"})

COVERAGE = "the formal obligation ledger"

# Key: repository-relative path. Value: why no lane runs it, and what does cover
# the behaviour instead. An entry is a decision someone made and can be argued
# with; it is not permission to leave a script rotting unexamined.
UNSELECTED_EXECUTABLES: dict[str, str] = {
    "e2e/k3d/cold_start_race_e2e.sh": (
        "Needs a multi-replica k3d cluster with its own disposable Postgres per "
        "phase, which no lane provisions: the k3d group runs the distributed "
        "control and NATS wake runners on one cluster. The migration race it "
        "guards is covered non-distributedly by the scoped-migration advisory "
        "lock tests and by pg_tests.sh's awaken-store-schema apply suite. "
        "Registered rather than deleted so the concurrency gap is written down."
    ),
    "e2e/k3d/failover_e2e.sh": (
        "Needs a multi-node k3d cluster; no lane provisions one. Cross-node "
        "history service is exercised by the distributed control runner the k3d "
        "group does run; node deletion mid-run is not."
    ),
    "e2e/k3d/worker_failover_e2e.sh": (
        "Needs a multi-node k3d cluster; no lane provisions one. Worker "
        "reclamation of a crashed peer's in-flight runs is covered at unit and "
        "Postgres level by the run-ingress pool and claim suites; the pod-level "
        "kill is not."
    ),
    "e2e/config_reload_e2e.sh": (
        "Starts a real server and rewrites its configuration on disk; the "
        "deterministic e2e lane drives configuration through the management API "
        "instead, so this file-watch path has no lane. Kept as the only "
        "executable statement of the on-disk reload contract."
    ),
    "e2e/graceful_drain_e2e.sh": (
        "Starts a real server and signals it mid-run; the deterministic e2e "
        "lane covers drain through the management API's shutdown path. The "
        "signal-driven drain has no lane."
    ),
    "e2e/k6/run.sh": (
        "A load profile, not a correctness gate: k6 is not a declared gate tool "
        "and a throughput number is not a pass/fail this repository asserts."
    ),
    "e2e/k6/run_durable_load.sh": (
        "Same load profile, durable variant. Same reason: no lane asserts a "
        "throughput number."
    ),
}

# Files that are reached by a lane but whose contents are not a lane. This
# registry names scripts as *data* -- the reason each one is unrun -- and a
# checker that lists what nothing runs must not thereby make everything look
# run. Its own entries are the one self-reference the closure cannot follow.
CLOSURE_OPAQUE = frozenset({"scripts/ci/check_lane_reachability.py"})

# A repository path mentioned in a command position.
REPOSITORY_PATH = re.compile(
    r"(?:scripts|e2e|deploy|web|packages|xtask|formal|contracts)/[\w./-]+\.(?:sh|py|mjs|ts|js)"
)
# Inside a package manifest the scripts name their files without a directory.
BARE_FILE = re.compile(r"[\w./-]+\.(?:sh|py|mjs|ts|js)")
TRIPLE_QUOTE = re.compile(r'"""|\'\'\'')


def _uncommented(text: str, suffix: str) -> str:
    """The text with comments removed.

    A checker's docstring that *names* a runner is documentation, not a lane. It
    used to make `nats_wake_e2e.sh` look selected from `check_dependency_gates`
    prose, which is the same false green this file exists to refuse.
    """

    lines: list[str] = []
    inside_docstring = False
    for line in text.splitlines():
        stripped = line.strip()
        if suffix == ".py":
            quotes = len(TRIPLE_QUOTE.findall(stripped))
            if inside_docstring:
                if quotes % 2 == 1:
                    inside_docstring = False
                continue
            if stripped.startswith(('"""', "'''")) and quotes % 2 == 1:
                inside_docstring = True
                continue
        if stripped.startswith("#") or stripped.startswith("//"):
            continue
        lines.append(line)
    return "\n".join(lines)


def references(relative: str, text: str) -> set[str]:
    path = Path(relative)
    if path.suffix == ".json":
        found = set(REPOSITORY_PATH.findall(text))
        parent = path.parent.as_posix()
        for token in BARE_FILE.findall(text):
            if "/" not in token:
                found.add(f"{parent}/{token}" if parent != "." else token)
        return found
    return set(REPOSITORY_PATH.findall(_uncommented(text, path.suffix)))


def selected(root: Path, lane_roots: tuple[str, ...]) -> set[str]:
    """Every file reachable from a lane root."""

    reached: set[str] = set()
    pending = list(lane_roots)
    while pending:
        relative = pending.pop()
        if relative in reached:
            continue
        path = root / relative
        if not path.is_file():
            continue
        reached.add(relative)
        if relative in CLOSURE_OPAQUE:
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        for reference in references(relative, text):
            candidate = reference.lstrip("./")
            if candidate not in reached and (root / candidate).is_file():
                pending.append(candidate)
    return reached


def indexed_executables(root: Path) -> set[str] | None:
    # Windows/DrvFS may report every regular file as executable. Git's published
    # modes own the checkout contract; filesystem modes remain useful only for
    # non-Git self-test fixtures. A failed index read is never an empty corpus.
    if not (root / ".git").exists():
        return None
    output = formal_sources.git_output(root, "ls-files", "--stage", "-z")
    return executable_index_modes(output.decode("utf-8"))


def executable_index_modes(output: str) -> set[str]:
    found = set()
    for record in output.split("\0"):
        if record:
            metadata, path = record.split("\t", 1)
            if metadata.split()[0] == "100755":
                found.add(path)
    return found


def executables(root: Path, indexed: set[str] | None = None) -> list[str]:
    found: list[str] = []
    for directory in SUBJECT_DIRECTORIES:
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            relative_path = path.relative_to(root)
            if SKIPPED_PARTS.intersection(relative_path.parts):
                continue
            relative = relative_path.as_posix()
            executable = relative in indexed if indexed is not None else os.access(path, os.X_OK)
            if path.is_file() and executable:
                found.append(relative)
    return found


def evidence_paths(coverage: dict) -> set[str]:
    """Every path any requirement in the ledger offers as its evidence."""

    found: set[str] = set()

    def walk(node: object) -> None:
        if isinstance(node, dict):
            for key, value in node.items():
                if key in ("evidence", "production_entrypoints") and isinstance(value, list):
                    found.update(entry for entry in value if isinstance(entry, str))
                else:
                    walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(coverage)
    return found


def unselected_errors(reached: set[str], present: list[str]) -> list[str]:
    """L1 and L3 over the executable corpus."""

    errors: list[str] = []
    for relative in present:
        if relative in reached or relative in UNSELECTED_EXECUTABLES:
            continue
        errors.append(
            f"{relative}: no lane runs it. It is executable, it reads as coverage, and nothing "
            "would report it if it stopped working. Select it from a lane (check-all.sh, a "
            "lefthook command, a workflow, an existing runner) or register it in "
            "UNSELECTED_EXECUTABLES with the reason and what covers the behaviour instead."
        )
    for relative in sorted(UNSELECTED_EXECUTABLES):
        if relative in reached:
            errors.append(
                f"{relative}: UNSELECTED_EXECUTABLES says no lane runs it, but a lane now does; "
                "drop the stale entry so the registry keeps naming only real gaps"
            )
        elif relative not in present:
            errors.append(
                f"{relative}: UNSELECTED_EXECUTABLES names a file that is not an executable here; "
                "drop the stale entry"
            )
    return errors


def evidence_errors(root: Path, reached: set[str], paths: set[str], indexed: set[str] | None = None) -> list[str]:
    """L2: evidence that is an executable must actually be run."""

    errors = []
    for relative in sorted(paths):
        path = root / relative
        executable = relative in indexed if indexed is not None else os.access(path, os.X_OK)
        if not path.is_file() or not executable:
            continue
        if relative in reached:
            continue
        errors.append(
            f"{COVERAGE} cites {relative} as evidence, but no lane runs it, so the ledger "
            "claims a requirement is true on the strength of a script that never executes. "
            "A registry entry cannot excuse this: either select it from a lane or stop citing it."
        )
    return errors


def validate(root: Path) -> list[str]:
    lane_roots = LANE_ROOTS + tuple(
        path.relative_to(root).as_posix() for path in sorted(root.glob(WORKFLOW_GLOB))
    )
    reached = selected(root, lane_roots)
    indexed = indexed_executables(root)
    present = executables(root, indexed)
    if not present:
        return ["no executables found under e2e/, scripts/ or deploy/; the corpus collapsed"]
    errors = unselected_errors(reached, present)
    if formal_ledger.present(root):
        try:
            ledger = formal_ledger.load(root)
        except formal_ledger.LedgerError as error:
            return errors + [str(error)]
        errors += evidence_errors(root, reached, evidence_paths(ledger), indexed)
    return errors


def self_test() -> None:
    """Decision table, one row per shape the survey found.

    Causes:
      C1 an executable is reachable from a lane root
      C2 it is reachable only through another selected runner (transitively)
      C3 it is reachable only from a comment or a docstring
      C4 it is unreachable and registered with a reason
      C5 it is unreachable and unregistered
      C6 a registry entry names a file a lane now runs
      C7 a registry entry names a file that is not an executable here
      C8 the ledger cites an executable a lane runs
      C9 the ledger cites an executable no lane runs
      C10 the ledger cites a non-executable source file
    Effects: E1 admitted, E2 rejected by name.
    Rules: L1 = C1|C2|C4 => E1; C3|C5 => E2.
           L2 = C8|C10 => E1; C9 => E2.
           L3 = C6|C7 => E2.
    """

    import tempfile  # noqa: PLC0415 - only the self-test writes fixtures

    # P1 Git executable mode is admitted; P2 ordinary source/JSON and symlinks
    # are not executables, even on a mount that grants X_OK to every file.
    assert executable_index_modes("100755 abc 0\tscripts/run.sh\0" +
        "100644 def 0\tscripts/source.py\0" + "120000 ghi 0\tscripts/link\0") == {"scripts/run.sh"}

    with tempfile.TemporaryDirectory() as temporary:
        # The root may itself live below a target/cache directory. Only paths
        # inside this root, never its ancestors, belong to the exclusion rule.
        root = Path(temporary) / "target" / "fixture"
        (root / "scripts/ci").mkdir(parents=True)
        (root / "e2e/k3d").mkdir(parents=True)
        fixture_index: set[str] = set()

        def write(relative: str, body: str, executable: bool = True) -> None:
            path = root / relative
            path.write_text(body, encoding="utf-8")
            if executable:
                path.chmod(0o755)
                fixture_index.add(relative)

        write("scripts/ci/check-all.sh", "run e2e x e2e/direct.sh\n", executable=False)
        write("e2e/direct.sh", "bash e2e/k3d/transitive.sh\n")
        write("e2e/k3d/transitive.sh", "true\n")
        write("scripts/ci/mentions.py", '"""Prose that names e2e/orphan.sh."""\nprint(1)\n')
        write("e2e/orphan.sh", "true\n")
        write("e2e/registered.sh", "true\n")
        for ignored in ("target", "node_modules", "__pycache__", ".git"):
            relative = f"e2e/{ignored}/nested.sh"
            (root / relative).parent.mkdir(parents=True)
            write(relative, "true\n")

        lane_roots = ("scripts/ci/check-all.sh",)
        reached = selected(root, lane_roots)
        assert "e2e/direct.sh" in reached, "C1"
        assert "e2e/k3d/transitive.sh" in reached, "C2"
        assert "e2e/orphan.sh" not in reached, "C3 a docstring mention is not a lane"

        # The published mode is the authority on Windows too; X_OK can be true
        # for every file in a temporary fixture without a real Git index.
        present = executables(root, fixture_index)
        assert "e2e/orphan.sh" in present, "C12 ancestor target is not an exclusion"
        assert not any("nested.sh" in name for name in present), "C12 descendants remain excluded"
        errors = unselected_errors(reached, present)
        assert not [e for e in errors if "e2e/direct.sh" in e], "C1/E1"
        assert not [e for e in errors if "transitive.sh" in e], "C2/E1"
        assert [e for e in errors if "e2e/orphan.sh" in e], "C3/E2 and C5/E2"

        UNSELECTED_EXECUTABLES["e2e/registered.sh"] = "fixture"
        try:
            errors = unselected_errors(reached, present)
            assert not [e for e in errors if "registered.sh" in e], "C4/E1"
            UNSELECTED_EXECUTABLES["e2e/direct.sh"] = "fixture"
            errors = unselected_errors(reached, present)
            assert [e for e in errors if "direct.sh" in e and "stale" in e], "C6/E2"
            del UNSELECTED_EXECUTABLES["e2e/direct.sh"]
            UNSELECTED_EXECUTABLES["e2e/gone.sh"] = "fixture"
            errors = unselected_errors(reached, present)
            assert [e for e in errors if "gone.sh" in e], "C7/E2"
            del UNSELECTED_EXECUTABLES["e2e/gone.sh"]
        finally:
            UNSELECTED_EXECUTABLES.pop("e2e/registered.sh", None)
            UNSELECTED_EXECUTABLES.pop("e2e/direct.sh", None)
            UNSELECTED_EXECUTABLES.pop("e2e/gone.sh", None)

        write("e2e/plain.rs", "fn main() {}\n", executable=False)
        assert not evidence_errors(root, reached, {"e2e/direct.sh"}, fixture_index), "C8/E1"
        assert evidence_errors(root, reached, {"e2e/orphan.sh"}, fixture_index), "C9/E2 the reported defect"
        assert not evidence_errors(root, reached, {"e2e/plain.rs"}, fixture_index), "C10/E1"
        assert evidence_errors(root, reached, {"e2e/plain.rs"}, {"e2e/plain.rs"}), "C11 executable source cannot hide behind its extension"

        ledger = {"features": [{"requirements": [{"evidence": ["e2e/orphan.sh", "e2e/plain.rs"]}]}]}
        assert evidence_paths(ledger) == {"e2e/orphan.sh", "e2e/plain.rs"}, "ledger walk"

    print("check_lane_reachability self-test: the decision table holds")


def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_lane_reachability.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
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
        f"OK - every executable under {', '.join(SUBJECT_DIRECTORIES)} is run by a lane or "
        f"registered with a reason ({len(UNSELECTED_EXECUTABLES)} registered), and every "
        "executable the proof ledger cites is run."
    )
