"""Hold every checker to the trigger its lefthook command is rendered from.

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

The trigger compared against is ``generate_hooks.trigger_patterns``: the
checker's ``INPUT_GLOBS``, its own path and every ``scripts/ci`` module it
imports. Run without arguments, this module holds every ``pre-commit``
command's Python checkers to it (the full gate's static group runs it): each
invocation in ``lefthook.yml`` runs in its own interpreter under the audit
hook, with ``{staged_files}`` replaced by every tracked file its trigger
matches and write flags (``--stage``, ``--tighten``) dropped, so the audit never
writes. A checker that starts a tool with no TOOL_INPUTS row is not failed for
it there: that tool runs in another process the hook cannot see, and the
checker's own reads are still compared.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from functools import lru_cache
from pathlib import Path

import generate_hooks
from generate_hooks import glob_matches

# The self-test runs in the hook-globs pre-commit command; the full audit
# (no argument) runs in the full gate, because it runs every hooked checker.
INPUT_GLOBS: tuple[str, ...] = ("lefthook.yml", "scripts/ci/*.py")

ROOT = Path(__file__).resolve().parents[2]
# Flags that make a checker write a baseline or the index; the audit drops them.
WRITE_FLAGS = frozenset({"--stage", "--tighten"})
STAGED_FILES = "{staged_files}"


def _repository_environment() -> dict[str, str]:
    """The environment without Git's repository overrides.

    A hook exports GIT_DIR and GIT_INDEX_FILE for the commit in progress; a
    `git` call that inherits them acts on that repository, not on the one its
    working directory names (`git init` then rewrites its `core.bare`).
    """
    return {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}

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

    def escapes(self, *, unlisted_tools: bool = True) -> list[str]:
        covered = _matcher(self.declared)
        errors = []
        for relative in sorted(self.opened):
            if SKIPPED_PARTS & set(relative.split("/")) or not (self.root / relative).is_file():
                continue
            if not covered(relative):
                errors.append(f"read {relative}, which INPUT_GLOBS does not cover")
        for tool in sorted(self.tools):
            if tool not in TOOL_INPUTS:
                if not unlisted_tools:
                    continue
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


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="_input_closure.py", description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    options = parser.parse_args(argv)
    if options.self_test:
        selftest()
        return 0
    errors = audit()
    for error in errors:
        print(f"ERROR: {error}; add the path to the checker's INPUT_GLOBS and run "
              "python3 scripts/ci/generate_hooks.py", file=sys.stderr)
    if errors:
        return 1
    print("OK - every pre-commit checker reads only what its rendered trigger covers.")
    return 0


def invocations(hooks: str) -> list[tuple[str, list[str]]]:
    """(command, argv) of every literal ``python3 scripts/ci/<checker>.py`` call in a pre-commit run body."""

    lines, spans = generate_hooks._spans(hooks)
    found = []
    for command, header, end in spans:
        block = "".join(generate_hooks._without_generated(
            lines[header + 1:generate_hooks._body_end(lines, header + 1, end)]))
        run = re.search(r"(?m)^\s*run:\s*(.*)$", block)
        if run is None:
            continue
        for part in re.split(r"&&|;", run.group(1)):
            words = shlex.split(part)
            if len(words) >= 2 and words[0] == "python3" and re.fullmatch(r"scripts/ci/\w+\.py", words[1]):
                found.append((command, words[1:]))
    return found


def _confined_run(root: Path, patterns: list[str], argv: list[str]) -> dict:
    """Run one checker in a fresh interpreter under the audit hook; its escapes and exit status."""

    with tempfile.TemporaryDirectory() as directory:
        report = Path(directory) / "report.json"
        bootstrap = (
            "import json, runpy, sys\n"
            "sys.path.insert(0, sys.argv[1] + '/scripts/ci')\n"
            "import _input_closure, pathlib\n"
            "root, patterns, report, script = sys.argv[1], json.loads(sys.argv[2]), sys.argv[3], sys.argv[4]\n"
            "sys.argv = sys.argv[4:]\n"
            "status = 0\n"
            "with _input_closure.Confinement(pathlib.Path(root), tuple(patterns)) as confinement:\n"
            "    try:\n"
            "        runpy.run_path(script, run_name='__main__')\n"
            "    except SystemExit as exit:\n"
            "        status = exit.code if isinstance(exit.code, int) else (0 if exit.code is None else 1)\n"
            "pathlib.Path(report).write_text(json.dumps(\n"
            "    {'status': status, 'escapes': confinement.escapes(unlisted_tools=False)}))\n"
        )
        subprocess.run(
            [sys.executable, "-c", bootstrap, str(root), json.dumps(patterns), str(report), *argv],
            cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            check=False,
        )
        if not report.is_file():
            return {"status": None, "escapes": ["ended before its reads could be reported"]}
        return json.loads(report.read_text(encoding="utf-8"))


def audit(root: Path = ROOT, jobs: int = 4) -> list[str]:
    """Every read of every pre-commit Python checker outside the trigger that runs it."""

    hooks = (root / "lefthook.yml").read_text(encoding="utf-8")
    declarations = generate_hooks.declared_input_globs(root / "scripts/ci")
    tracked = subprocess.run(["git", "ls-files", "-z"], cwd=root, capture_output=True, check=True,
                             text=True, env=_repository_environment()).stdout.split("\0")
    runs = []
    for command, argv in invocations(hooks):
        name = Path(argv[0]).name
        if name == Path(__file__).name:
            continue  # its self-test starts fixture tools by name only, inside its own confinement
        if name not in declarations:
            continue  # generate_hooks.py --check already refuses an undeclared checker
        patterns = generate_hooks.trigger_patterns(name, declarations, root / "scripts/ci")
        if patterns == [generate_hooks.UNIVERSAL]:
            continue  # reads every staged file by declaration: nothing to escape to
        arguments = []
        for word in argv:
            if word == STAGED_FILES:
                covered = _matcher(tuple(patterns))
                arguments.extend(path for path in tracked if path and covered(path))
            elif word not in WRITE_FLAGS:
                arguments.append(word)
        runs.append((command, name, patterns, arguments))
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        results = list(pool.map(lambda run: _confined_run(root, run[2], run[3]), runs))
    errors = []
    for (command, name, _, arguments), result in zip(runs, results):
        for escape in result["escapes"]:
            errors.append(f"pre-commit `{command}`: {name} {escape}")
    return errors


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
        assert run(("crates/**",), start("make")) and \
            not Confinement(root, ("crates/**",)).escapes(unlisted_tools=False), "I9"

        # A1-A4 over the whole audit, on a scratch repository with one hook.
        # A1 a checker reads only its trigger -> no error; A2 it reads a file
        # outside it -> one error naming command, checker and path; A3 its
        # trigger includes a helper it imports -> the helper read is no
        # escape; A4 {staged_files} expands to the tracked files the trigger
        # matches and a write flag is dropped.
        scripts = root / "scripts/ci"
        scripts.mkdir(parents=True, exist_ok=True)
        for module in ("generate_hooks.py", "_input_closure.py"):
            (scripts / module).write_text((Path(__file__).parent / module).read_text(encoding="utf-8"),
                                          encoding="utf-8")
        (scripts / "_helper.py").write_text("VALUE = 1\n", encoding="utf-8")
        (scripts / "check_fixture.py").write_text(
            'INPUT_GLOBS = ("crates/**",)\n'
            "import sys, pathlib\n"
            "def main():\n"
            "    import _helper\n"
            "    assert '--stage' not in sys.argv and 'crates/a/src/lib.rs' in sys.argv, sys.argv\n"
            "    pathlib.Path('crates/a/src/lib.rs').read_text()\n"
            "    if '--stray' in sys.argv:\n"
            "        pathlib.Path('docs/a.md').read_text()\n"
            "main()\n", encoding="utf-8")

        def hooks(*extra: str) -> None:
            (root / "lefthook.yml").write_text(
                "pre-commit:\n  commands:\n    fixture:\n"
                f"      run: python3 scripts/ci/check_fixture.py --stage {' '.join(extra)} {{staged_files}}\n",
                encoding="utf-8")

        # A5 a pre-commit hook exports GIT_DIR, GIT_WORK_TREE and
        # GIT_INDEX_FILE for the commit in progress: the fixture's git calls
        # and the audit must leave that outer repository byte-identical (a
        # `git init` that inherited GIT_DIR once set the shared `core.bare`).
        from unittest import mock

        with tempfile.TemporaryDirectory() as outer_directory:
            outer = Path(outer_directory) / "outer"
            subprocess.run(["git", "init", "-q", str(outer)], check=True, env=_repository_environment())
            outer_git = outer / ".git"

            def state() -> dict[str, bytes]:
                return {relative: (outer_git / relative).read_bytes()
                        for relative in ("config", "HEAD") if (outer_git / relative).is_file()} | {
                    "index": (outer_git / "index").read_bytes() if (outer_git / "index").is_file() else b""}

            before = state()
            exported = {"GIT_DIR": str(outer_git), "GIT_WORK_TREE": str(outer),
                        "GIT_INDEX_FILE": str(outer_git / "index")}
            with mock.patch.dict(os.environ, exported):
                isolated = _repository_environment()
                assert not any(key.startswith("GIT_") for key in isolated), "A5 overrides stripped"
                subprocess.run(["git", "init", "-q"], cwd=root, check=True, env=isolated)
                subprocess.run(["git", "add", "crates", "docs"], cwd=root, check=True, env=isolated)
                hooks()
                assert audit(root, jobs=1) == [], f"A1/A3/A4 {audit(root, jobs=1)}"
                hooks("--stray")
                assert audit(root, jobs=1) == [
                    "pre-commit `fixture`: check_fixture.py read docs/a.md, which INPUT_GLOBS does not cover"
                ], "A2"
            assert state() == before, "A5 the exported outer repository is unchanged"
    print("_input_closure selftest: the decision table holds")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
