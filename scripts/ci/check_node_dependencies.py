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
is genuinely missing from a manifest.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = "pnpm-workspace.yaml"

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
        if not (root / relative).is_dir():
            continue
        missing = missing_dependencies(root, relative)
        if not missing:
            continue
        shown = ", ".join(missing[:4])
        if len(missing) > 4:
            shown += f", and {len(missing) - 4} more"
        errors.append(
            f"{relative}: {len(missing)} declared dependencies are not installed "
            f"({shown}); run: {install_command(root, relative, members)}"
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
        assert any(error.startswith("web:") and "pnpm install" in error for error in errors), (
            "D2 a workspace member is pnpm's to install"
        )
        assert any(
            error.startswith("outside:") and "npm --prefix outside install" in error
            for error in errors
        ), "D2 a root outside the workspace installs itself"
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
        assert len(adopted) == 1 and "pnpm install" in adopted[0], f"advice follows adoption: {adopted}"
    print("check_node_dependencies self-test: the decision table and the advice hold")


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
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


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
