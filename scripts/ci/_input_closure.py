"""Hold a checker to the INPUT_GLOBS its lefthook trigger is rendered from.

A checker's hook runs only when a staged path matches the glob rendered from
its INPUT_GLOBS. A declaration narrower than what the checker really reads is
a gate that silently stops running for the paths it forgot; the widest
declaration (``**/*``) avoids that but makes every commit, docs included, pay
for the check. This module removes the trade: while a checker runs, every
repository file it opens and every tool it starts is compared with its
declaration, and anything outside it is an error naming the path. The
declaration can then be as narrow as the real inputs, because it cannot fall
behind them without the checker itself failing.

What counts as a read:

* a regular repository file opened by this process (Python's ``open`` audit
  event, which also covers module imports); bytecode caches are derived output
  and do not count, and a path that does not exist was not read;
* a tool this process starts, by the repository paths that tool is known to
  read (TOOL_INPUTS). A tool with no row is an error: what it reads has to be
  written down before a narrow trigger can be trusted.

Directory listings are not reads. Every directory a checker walks it walks to
open the files in it, and those opens are what this compares.
"""

from __future__ import annotations

import os
import sys
from functools import lru_cache
from pathlib import Path

from generate_hooks import glob_matches

# Repository paths each tool reads when a checker starts it, so its inputs are
# held to the declaration like the checker's own opens.
TOOL_INPUTS: dict[str, tuple[str, ...]] = {
    # Workspace resolution: the root manifest and lockfile, the toolchain pin,
    # and Cargo configuration. Member manifests are read by the checkers
    # themselves before they ask Cargo anything, so they are already compared.
    "cargo": ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"),
    # Committed objects only (HEAD, ls-tree, cat-file): a staged change is not
    # what git reports until it is committed, so it adds no trigger input.
    "git": (),
}

SKIPPED_PARTS = frozenset({"__pycache__", ".git"})


class Confinement:
    """Records reads outside ``declared`` while active; ``escapes()`` lists them."""

    def __init__(self, root: Path, declared: tuple[str, ...]) -> None:
        self.root = root.resolve()
        self.declared = declared
        self.active = False
        self.opened: set[str] = set()
        self.tools: set[str] = set()

    def _relative(self, path: object) -> str | None:
        try:
            absolute = os.path.abspath(os.fsdecode(path))  # type: ignore[arg-type]
        except TypeError:
            return None  # an open by file descriptor names no path
        prefix = str(self.root) + os.sep
        if not absolute.startswith(prefix):
            return None
        return absolute[len(prefix):].replace(os.sep, "/")

    def audit(self, event: str, arguments: tuple) -> None:
        if not self.active:
            return
        if event == "open":
            relative = self._relative(arguments[0])
            if relative is not None:
                self.opened.add(relative)
        elif event == "subprocess.Popen":
            command = arguments[1]
            first = command[0] if isinstance(command, (list, tuple)) else str(command).split()[0]
            self.tools.add(Path(os.fsdecode(first)).name)

    def __enter__(self) -> "Confinement":
        sys.addaudithook(self.audit)
        self.active = True
        return self

    def __exit__(self, *_: object) -> None:
        # An audit hook cannot be removed; it goes quiet instead.
        self.active = False

    def escapes(self) -> list[str]:
        covered = _matcher(self.declared)
        errors = []
        for relative in sorted(self.opened):
            if SKIPPED_PARTS & set(relative.split("/")) or not (self.root / relative).is_file():
                continue
            if not covered(relative):
                errors.append(f"read {relative}, which INPUT_GLOBS does not cover")
        for tool in sorted(self.tools):
            if tool not in TOOL_INPUTS:
                errors.append(
                    f"started {tool}, which has no TOOL_INPUTS row in scripts/ci/_input_closure.py "
                    "saying which repository paths it reads"
                )
                continue
            for relative in TOOL_INPUTS[tool]:
                if not covered(relative):
                    errors.append(f"started {tool}, which reads {relative}; INPUT_GLOBS does not cover it")
        return errors


def _matcher(declared: tuple[str, ...]):
    @lru_cache(maxsize=None)
    def covered(relative: str) -> bool:
        return any(glob_matches(pattern, relative) for pattern in declared)

    return covered


def report(checker: str, errors: list[str]) -> None:
    for error in errors:
        print(
            f"ERROR: {checker} {error}; add the path to its INPUT_GLOBS and run "
            "python3 scripts/ci/generate_hooks.py",
            file=sys.stderr,
        )


def selftest() -> None:
    """Cause/effect decision table over a scratch repository.

    | Rule | the checker                              | effect                     |
    |------|------------------------------------------|----------------------------|
    | I1   | opens a file its declaration covers      | no error                   |
    | I2   | opens a file outside its declaration     | error naming the path      |
    | I3   | opens a path that does not exist         | no error (nothing read)    |
    | I4   | writes a bytecode cache                  | no error (derived output)  |
    | I5   | starts a tool whose inputs are covered   | no error                   |
    | I6   | starts a tool whose inputs are not       | error naming tool and path |
    | I7   | starts a tool with no TOOL_INPUTS row    | error naming the tool      |
    | I8   | reads after the confinement ended        | not recorded               |
    """
    import subprocess
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for relative in ("crates/a/src/lib.rs", "docs/a.md", "scripts/ci/__pycache__/x.pyc"):
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / relative).write_text("x\n", encoding="utf-8")

        def run(declared: tuple[str, ...], action) -> list[str]:
            with Confinement(root, declared) as confinement:
                action()
            return confinement.escapes()

        read = lambda relative: (lambda: (root / relative).read_text(encoding="utf-8"))
        assert run(("crates/**",), read("crates/a/src/lib.rs")) == [], "I1"
        assert run(("crates/**",), read("docs/a.md")) == [
            "read docs/a.md, which INPUT_GLOBS does not cover"
        ], "I2"

        def missing() -> None:
            try:
                (root / "docs/absent.md").read_text(encoding="utf-8")
            except FileNotFoundError:
                pass

        assert run(("crates/**",), missing) == [], "I3"
        assert run(("crates/**",), read("scripts/ci/__pycache__/x.pyc")) == [], "I4"
        def start(tool: str):
            # The audit event fires before exec, so a tool that is not there
            # still counts as started and nothing real is run (cargo here is an
            # admission wrapper that could queue behind the host's build lane).
            def action() -> None:
                try:
                    subprocess.run([str(root / "absent-bin" / tool), "--version"], check=False)
                except FileNotFoundError:
                    pass

            return action

        assert run(("crates/**",), start("git")) == [], "I5"
        cargo_errors = run(("crates/**",), start("cargo"))
        assert cargo_errors and all(error.startswith("started cargo, which reads ") for error in cargo_errors), "I6"
        cargo_inputs = ("crates/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**")
        assert run(cargo_inputs, start("cargo")) == [], "I6 covered"
        assert run(("**/*",), start("make"))[0].startswith("started make, which has no"), "I7"
        with Confinement(root, ("crates/**",)) as confinement:
            pass
        read("docs/a.md")()
        assert confinement.escapes() == [], "I8"
    print("_input_closure selftest: the decision table holds")
