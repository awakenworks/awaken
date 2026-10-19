"""Admission and process regressions for the Git hook entry point."""

from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess
import tempfile

from _native_shell import bash_executable


HOOK_ENTRY = "scripts/ci/run_lefthook.sh"
HOOK_COMMAND = (
    "$(if grep -q '^lefthook:' lefthook.yml; then "
    f"printf 'sh {HOOK_ENTRY}'; else printf lefthook; fi)"
)


def hook_entry_errors(hooks: str) -> list[str]:
    if re.findall(r"(?m)^lefthook:\s*(\S[^\n]*)$", hooks) != [HOOK_COMMAND]:
        return ["Git hooks must use the repository entry point so push checks cannot skip on a guessed empty diff"]
    return []


def self_test(root: Path) -> None:
    hooks = (root / "lefthook.yml").read_text(encoding="utf-8")
    assert not hook_entry_errors(hooks), "E1 repository hook entry"
    for replacement in ("", "lefthook: lefthook", f"lefthook: sh {HOOK_ENTRY}.old"):
        assert hook_entry_errors(re.sub(r"(?m)^lefthook:.*$", replacement, hooks)), "E2 missing/wrong entry"

    # Each process receives the real entry point. The stand-in records the
    # child protocol, not the routing decision: arguments, stdin, status and
    # invocation count are observed at the external-tool boundary.
    payload = "HEAD abc refs/heads/main def\nrefs/heads/next ghi refs/heads/next jkl\n"
    cases = (
        ("push", ("run", "pre-push", "origin", "ssh://host/repo with spaces"), payload, 0, True),
        ("commit", ("run", "pre-commit"), "", 0, False),
        ("message", ("run", "commit-msg", "message with spaces.txt"), "", 0, False),
        ("reference", ("run", "reference-transaction", "prepared"), payload, 0, False),
        ("failed-push", ("run", "pre-push", "origin", "remote"), payload, 37, True),
    )
    with tempfile.TemporaryDirectory(prefix="hook entry ") as directory:
        fixture = Path(directory)
        binary = fixture / "lefthook"
        binary.write_text(
            '#!/bin/sh\n'
            'printf "%s\\0" "$@" >> "$FIXTURE_ARGS"\n'
            'cat > "$FIXTURE_STDIN"\n'
            'exit "$FIXTURE_STATUS"\n',
            encoding="utf-8",
        )
        binary.chmod(0o755)
        environment = {**os.environ, "PATH": str(fixture) + os.pathsep + os.environ["PATH"]}
        for label, arguments, stdin, status, forced in cases:
            args_file, stdin_file = fixture / (label + ".args"), fixture / (label + ".stdin")
            environment.update(FIXTURE_ARGS=args_file.as_posix(), FIXTURE_STDIN=stdin_file.as_posix(), FIXTURE_STATUS=str(status))
            result = subprocess.run(
                [bash_executable(), str(root / HOOK_ENTRY), *arguments],
                env=environment, input=stdin.encode(), capture_output=True, timeout=15,
            )
            assert result.returncode == status, (label, result.returncode, result.stderr)
            actual = args_file.read_bytes().decode().split("\0")[:-1]
            assert actual == [*arguments, *(["--force"] if forced else [])], (label, actual)
            assert stdin_file.read_bytes() == stdin.encode(), label
        for arguments in ((), ("run",), ("--awaken-gate-argument-probe",)):
            before = args_file.read_bytes()
            result = subprocess.run(
                [bash_executable(), str(root / HOOK_ENTRY), *arguments],
                env=environment, input=b"", capture_output=True, timeout=15,
            )
            assert result.returncode == 2 and b"usage: run_lefthook.sh" in result.stderr, arguments
            assert args_file.read_bytes() == before, "invalid arguments must not execute a child"
        # Generated hooks are shared by all worktrees. Exercise the configured
        # command with an old checkout, a broken current checkout and a complete
        # current checkout. Only the first may use the original tool directly.
        config = fixture / "lefthook.yml"
        arguments = ("run", "pre-push", "origin", "remote with spaces")
        for label in ("old", "missing", "current"):
            config.write_text("output: [failure]\n" if label == "old" else f"lefthook: {HOOK_COMMAND}\n", encoding="utf-8")
            if label == "current":
                entry = fixture / HOOK_ENTRY
                entry.parent.mkdir(parents=True)
                entry.write_bytes((root / HOOK_ENTRY).read_bytes())
            args_file, stdin_file = fixture / (label + ".args"), fixture / (label + ".stdin")
            environment.update(FIXTURE_ARGS=args_file.as_posix(), FIXTURE_STDIN=stdin_file.as_posix(), FIXTURE_STATUS="0")
            result = subprocess.run(
                [bash_executable(), "-c", f'exec {HOOK_COMMAND} "$@"', "hook", *arguments],
                cwd=fixture, env=environment, input=payload.encode(), capture_output=True, timeout=15,
            )
            if label == "missing":
                assert result.returncode != 0 and not args_file.exists(), "required hook entry cannot fail open"
            else:
                assert result.returncode == 0, (label, result.stderr)
                expected = [*arguments, *(["--force"] if label == "current" else [])]
                assert args_file.read_bytes().decode().split("\0")[:-1] == expected, label
                assert stdin_file.read_bytes() == payload.encode(), label
    print("hook entrypoint: 11 process cases passed (routing, failure, refusals and shared-worktree compatibility)")
