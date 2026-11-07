#!/usr/bin/env python3
"""Say that Node dependencies are not installed, instead of failing as a test.

This repository installs JavaScript in two places that look like one.
``pnpm-workspace.yaml`` lists ``web`` and ``packages/*``; ``e2e`` is deliberately
outside it, because the Managed SDK oracle depends on npm's exact lockfile
resolution and on owning its own ``node_modules`` boundary. The consequence is a
trap: in a fresh worktree ``pnpm install`` reports "Already up to date" while
``e2e/node_modules`` is empty, and the next gate dies with
``ERR_MODULE_NOT_FOUND: @modelcontextprotocol/sdk`` — which reads exactly like a
broken import in the repository rather than an uninstalled dependency.

So the missing install is checked, and it names the command that fixes it. Which
command is *derived* from ``pnpm-workspace.yaml`` rather than assumed: if a root
is a workspace member it is pnpm's to install, and otherwise it installs itself.
Adding ``e2e`` to the workspace one day would change this checker's advice
without anybody editing it.

Nothing here installs anything: a gate must still fail by name when a dependency
is genuinely missing from a manifest. ``scripts/ci/doctor.sh --fix`` is the one
command that installs, and the lockfile command it runs for each root is derived
here (``lockfile_install_command``) beside the advice, from the same workspace file.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = "pnpm-workspace.yaml"
# The one command that installs every root from its lockfile; gate_tools.py's
# bootstrap runs ``lockfile_install_command`` for each root that is missing.
FIX_COMMAND = "scripts/ci/doctor.sh --fix"

# Roots whose dependencies a gate imports. Each must either be installed or say
# how to install it; a root absent from the tree is simply not checked.
DEPENDENCY_ROOTS = ("e2e", "web", ".")


def workspace_patterns(root: Path) -> list[str]:
    """The ``packages:`` globs of pnpm-workspace.yaml, or [] when it has none.

    Parsed with a narrow reader rather than a YAML dependency: this runs before
    anything is installed, so it cannot rely on a package being available.
    """

    manifest = root / WORKSPACE
    if not manifest.exists():
        return []
    patterns: list[str] = []
    inside = False
    for line in manifest.read_text(encoding="utf-8").splitlines():
        if re.fullmatch(r"packages:\s*", line):
            inside = True
            continue
        if inside:
            item = re.fullmatch(r"\s*-\s*['\"]?([^'\"#]+?)['\"]?\s*", line)
            if item:
                patterns.append(item.group(1))
                continue
            if line.strip() and not line.startswith((" ", "\t")):
                inside = False
    return patterns


def workspace_members(root: Path) -> set[Path]:
    """Directories pnpm installs as part of the workspace."""

    members: set[Path] = set()
    for pattern in workspace_patterns(root):
        for match in root.glob(pattern):
            if (match / "package.json").exists():
                members.add(match.resolve())
    return members


def declared_dependencies(manifest: Path) -> list[str]:
    """Every runtime and development dependency one package.json names."""

    document = json.loads(manifest.read_text(encoding="utf-8"))
    names: list[str] = []
    for field in ("dependencies", "devDependencies"):
        section = document.get(field)
        if isinstance(section, dict):
            names.extend(section)
    return sorted(set(names))


def install_command(root: Path, relative: str, members: set[Path]) -> str:
    """The command that installs one root, decided by the workspace itself."""

    if relative == "." or (root / relative).resolve() in members:
        return "pnpm install"
    return f"npm --prefix {relative} install"


def lockfile_install_command(root: Path, relative: str, members: set[Path],
                             *, online: bool = False) -> list[str]:
    """The lockfile-exact install of one root; offline unless the caller opts in.

    | Rule | root                    | online | command                                   |
    |------|-------------------------|--------|-------------------------------------------|
    | L1   | workspace member or `.` | no     | pnpm install --offline --frozen-lockfile  |
    | L2   | workspace member or `.` | yes    | pnpm install --frozen-lockfile            |
    | L3   | outside the workspace   | no     | npm ci --prefix <root> --offline ...      |
    | L4   | outside the workspace   | yes    | npm ci --prefix <root> ...                |

    Neither form rewrites a lockfile: ``npm ci`` refuses a drifted one and
    ``--frozen-lockfile`` makes pnpm refuse too. A workspace member is installed
    from the repository root, because that is where pnpm's lockfile lives.
    """

    if install_command(root, relative, members) == "pnpm install":
        return ["pnpm", "install", "--frozen-lockfile", *(() if online else ("--offline",))]
    return ["npm", "ci", "--prefix", relative, "--no-audit", "--no-fund",
            *(() if online else ("--offline",))]


def missing_summary(root: Path, relative: str) -> str:
    """What is missing under one root, or "" when nothing is."""

    missing = missing_dependencies(root, relative) if (root / relative).is_dir() else []
    if not missing:
        return ""
    shown = ", ".join(missing[:4])
    if len(missing) > 4:
        shown += f", and {len(missing) - 4} more"
    return f"{len(missing)} declared dependencies are not installed ({shown})"


def missing_dependencies(root: Path, relative: str) -> list[str]:
    """Declared dependencies with no directory under this root's node_modules."""

    manifest = root / relative / "package.json"
    if not manifest.exists():
        return []
    modules = root / relative / "node_modules"
    return [
        name
        for name in declared_dependencies(manifest)
        if not (modules / name).exists()
    ]


def dependency_errors(root: Path, roots: tuple[str, ...]) -> list[str]:
    """Cause/effect for one package root:

    | Rule | declares dependencies | all installed | Effect                     |
    |------|-----------------------|---------------|----------------------------|
    | D1   | yes                   | yes           | accept                     |
    | D2   | yes                   | no            | reject, naming the command |
    | D3   | no                    | n/a           | accept without node_modules|
    | D4   | the root is absent    | n/a           | accept: nothing to install |
    """

    members = workspace_members(root)
    errors: list[str] = []
    for relative in roots:
        summary = missing_summary(root, relative)
        if not summary:
            continue
        lockfile = " ".join(lockfile_install_command(root, relative, members))
        errors.append(
            f"{relative}: {summary}; run: {FIX_COMMAND} "
            f"(it runs `{lockfile}` from the local package cache)"
        )
    return errors


def selftest() -> None:
    """Prove the table, and prove the advice follows the workspace file."""

    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / WORKSPACE).write_text("packages:\n  - web\n  - packages/*\n", encoding="utf-8")
        (root / "package.json").write_text('{"name":"root"}', encoding="utf-8")
        for relative, dependencies in (
            ("web", ["react"]),
            ("outside", ["@scope/sdk", "tap"]),
            ("plain", []),
        ):
            (root / relative).mkdir(parents=True)
            section = {name: "1.0.0" for name in dependencies}
            (root / relative / "package.json").write_text(
                json.dumps({"name": relative, "dependencies": section}), encoding="utf-8"
            )
        roots = ("web", "outside", "plain", "absent")
        assert workspace_patterns(root) == ["web", "packages/*"], "the workspace file is read"

        errors = dependency_errors(root, roots)
        assert len(errors) == 2, f"D2 twice, D3 and D4 silent: {errors}"
        assert all(f"run: {FIX_COMMAND} " in error for error in errors), (
            f"D2 every refusal names the one fix command: {errors}"
        )
        assert any(
            error.startswith("web:") and "pnpm install --frozen-lockfile --offline" in error
            for error in errors
        ), "D2 a workspace member is pnpm's to install"
        assert any(
            error.startswith("outside:") and "npm ci --prefix outside" in error
            for error in errors
        ), "D2 a root outside the workspace installs itself"
        members = workspace_members(root)
        assert install_command(root, "outside", members) == "npm --prefix outside install"
        # L1-L4: offline unless asked, lockfile-exact either way.
        for relative, online, expected in (
            ("web", False, ["pnpm", "install", "--frozen-lockfile", "--offline"]),
            ("web", True, ["pnpm", "install", "--frozen-lockfile"]),
            ("outside", False, ["npm", "ci", "--prefix", "outside", "--no-audit", "--no-fund",
                                "--offline"]),
            ("outside", True, ["npm", "ci", "--prefix", "outside", "--no-audit", "--no-fund"]),
        ):
            actual = lockfile_install_command(root, relative, members, online=online)
            assert actual == expected, f"L1-L4 {relative}/{online}: {actual}"
        assert all("plain" not in error and "absent" not in error for error in errors), "D3 + D4"

        # D1: install them and the root falls silent.
        for relative, dependencies in (("web", ["react"]), ("outside", ["@scope/sdk", "tap"])):
            for name in dependencies:
                (root / relative / "node_modules" / name).mkdir(parents=True)
        assert not dependency_errors(root, roots), "D1 an installed root is silent"

        # The advice follows the workspace file: adopting `outside` into the
        # workspace makes it pnpm's, with no edit to this checker.
        (root / "outside" / "node_modules" / "tap").rmdir()
        (root / WORKSPACE).write_text(
            "packages:\n  - web\n  - packages/*\n  - outside\n", encoding="utf-8"
        )
        adopted = dependency_errors(root, roots)
        assert len(adopted) == 1 and "`pnpm install" in adopted[0], f"advice follows adoption: {adopted}"
    print("check_node_dependencies self-test: the decision table and the advice hold")


def main(arguments: argparse.Namespace) -> int:
    if arguments.self_test:
        selftest()
        return 0
    selftest()
    errors = dependency_errors(ROOT, DEPENDENCY_ROOTS)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"OK - Node dependencies are installed for {len(DEPENDENCY_ROOTS)} package roots.")
    return 0


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_node_dependencies.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
    )
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:])))
