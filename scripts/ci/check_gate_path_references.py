#!/usr/bin/env python3
"""Refuse a shell or YAML gate that names a crate file which no longer exists.

`scripts/ci/check-reliability.sh` pointed `cargo mutants --file` at
`crates/contract/awaken-agent-contract/src/clock/utc.rs` after a refactor had
moved that codec to `awaken-lease-contract`. Nothing looked: the script runs only
in the reliability lane, its Python self-test only in the full static group, and
the commit that moved the file touched no gate. The dangling path would have
surfaced as a confusing mutation-tool error hours later, on someone else's run.

Python gates do not share the hole: they read the files they name, so a moved
file fails them in the pre-commit hook that already triggers on crate sources.
Shell and YAML gates only *pass* the path to a tool that runs later. So the rule
is over those files, and over exactly one kind of reference -- a path to a
source or manifest file under `crates/` -- because that is what a refactor moves.
Gate outputs never live under `crates/`, so no reference here is legitimately
missing.

The scanned files are the shell/YAML patterns in INPUT_GLOBS. Crate paths
trigger re-runs when a referenced file moves; Python helpers are read as inputs,
while their contents remain outside this rule's shell/YAML corpus.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

import formal_sources

INPUT_GLOBS: tuple[str, ...] = (
    "crates/**",
    "scripts/ci/formal_sources.py",
    "scripts/**/*.sh",
    "e2e/**/*.sh",
    "deploy/**/*.sh",
    "deploy/**/*.yaml",
    "deploy/**/*.yml",
    ".github/**/*.yml",
    "lefthook.yml",
)

ROOT = Path(__file__).resolve().parents[2]
REFERENCE = re.compile(r"(?<![\w./-])crates/[a-z]+/[A-Za-z0-9_-]+/[A-Za-z0-9_./-]*?[A-Za-z0-9_]\.(?:rs|toml)\b")


def scanned_files(tracked: list[str]) -> list[str]:
    """Tracked files matched by INPUT_GLOBS, excluding the crate sources themselves."""

    patterns = [glob for glob in INPUT_GLOBS if not glob.startswith("crates/")
                and Path(glob).suffix in (".sh", ".yaml", ".yml")]
    selected = []
    for relative in tracked:
        path = Path(relative)
        if any(path.match(glob) or _matches_recursive(relative, glob) for glob in patterns):
            selected.append(relative)
    return sorted(set(selected))


def _matches_recursive(relative: str, glob: str) -> bool:
    # Path.match treats `**` as a single component; a `dir/**/*.ext` glob must
    # also admit files directly under `dir/` and at any depth below it.
    if "/**/" not in glob:
        return False
    prefix, suffix = glob.split("/**/", 1)
    return relative.startswith(prefix + "/") and Path(relative).match(suffix)


def dangling_references(root: Path, files: dict[str, str]) -> list[str]:
    errors: list[str] = []
    for relative, text in sorted(files.items()):
        for match in REFERENCE.finditer(text):
            target = match.group(0)
            if (root / target).is_file():
                continue
            line = text.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: names {target}, which does not exist; a gate that hands a "
                "moved path to a tool fails later and elsewhere, so point it at the file's "
                "current location in the commit that moved it"
            )
    return errors


def self_test() -> None:
    """Decision table.

    Causes: C1 the referenced crate file exists; C2 it does not; C3 the token is
    a longer path that merely contains a crate path (not a reference to it);
    C4 the file is not a gate (a crate source or a Markdown doc).
    Effects: E1 admitted; E2 rejected naming file:line.
    Rules: C1 => E1; C2 => E2; C3 => E1; C4 => not scanned.
    """

    import tempfile  # noqa: PLC0415 - only the self-test writes fixtures

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        real = root / "crates/contract/awaken-x/src/utc.rs"
        real.parent.mkdir(parents=True)
        real.write_text("")
        files = {
            "scripts/ci/good.sh": "cargo mutants --file crates/contract/awaken-x/src/utc.rs\n",
            "scripts/ci/moved.sh": "\n  --file crates/contract/awaken-x/src/clock/utc.rs \\\n",
            "scripts/ci/nested.sh": "cp target/crates/contract/awaken-x/src/gone.rs out\n",
        }
        errors = dangling_references(root, files)
        assert not [e for e in errors if e.startswith("scripts/ci/good.sh")], "C1/E1"
        assert [e for e in errors if e.startswith("scripts/ci/moved.sh:2:")], "C2/E2 names file:line"
        assert not [e for e in errors if e.startswith("scripts/ci/nested.sh")], "C3/E1"
        # Product/context nesting must keep both positive and missing-target
        # rows visible to the same source-reference matcher.
        deep = root / "crates/sdk/contracts/awaken-x/src/utc.rs"
        deep.parent.mkdir(parents=True)
        deep.write_text("")
        errors = dangling_references(root, {
            "scripts/ci/deep.sh": "cargo mutants --file crates/sdk/contracts/awaken-x/src/utc.rs\n",
            "scripts/ci/deep-missing.sh": "cargo mutants --file crates/platform/session/awaken-x/src/missing.rs\n",
        })
        assert not [e for e in errors if e.startswith("scripts/ci/deep.sh")], "C1 nested product path"
        assert [e for e in errors if e.startswith("scripts/ci/deep-missing.sh:1:")], "C2 nested missing target"
    tracked = [
        "scripts/ci/check-reliability.sh",
        "scripts/e2e/sub/run.sh",
        "lefthook.yml",
        ".github/workflows/quality.yml",
        "crates/contract/awaken-x/src/lib.rs",
        "docs/adr/0001.md",
        "scripts/ci/check_reliability.py",
    ]
    assert scanned_files(tracked) == [
        ".github/workflows/quality.yml",
        "lefthook.yml",
        "scripts/ci/check-reliability.sh",
        "scripts/e2e/sub/run.sh",
    ], f"C4 scope {scanned_files(tracked)}"
    # SX6: declaring an imported Python data input must trigger this guard
    # without widening its existing shell/YAML matcher corpus. Exercise the
    # actual declaration/filter, rather than a second path registry.
    from unittest.mock import patch
    with patch.dict(scanned_files.__globals__, INPUT_GLOBS=(*INPUT_GLOBS, "scripts/ci/formal_sources.py")):
        assert "scripts/ci/formal_sources.py" not in scanned_files(tracked + ["scripts/ci/formal_sources.py"]), "SX6 data input"
    print("check_gate_path_references self-test: the decision table holds")


def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_gate_path_references.py", description=__doc__, allow_abbrev=False,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    if parse(argv).self_test:
        self_test()
        return 0
    files = {
        relative: (ROOT / relative).read_text(encoding="utf-8", errors="replace")
        for relative in scanned_files(formal_sources.tracked_files(ROOT))
        if (ROOT / relative).is_file()
    }
    if len(files) < 20:
        print(
            f"ERROR: only {len(files)} gate files matched INPUT_GLOBS; a rule over an empty "
            "set proves nothing",
            file=sys.stderr,
        )
        return 1
    errors = dangling_references(ROOT, files)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        return 1
    references = sum(len(REFERENCE.findall(text)) for text in files.values())
    print(f"OK - {references} crate file references in {len(files)} shell/YAML gate files resolve.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
