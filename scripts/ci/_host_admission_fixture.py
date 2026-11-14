"""The fake host Cargo/admission policy the admission tests share.

Kept out of test_full_gate_admission.py so a test that borrows this fixture
does not inherit that test's whole input closure (generate_hooks.py
--script-tests follows imports), which made script edits run the borrower.
"""

import os
from pathlib import Path


def host_admission_fixture(root: Path) -> tuple[Path, dict[str, str]]:
    """A native Cargo that must never run, and a host admission policy.

    The policy waits ``FIXTURE_WAIT`` seconds, then refuses with a nonzero
    ``FIXTURE_STATUS`` or admits and runs the requested ``cargo-*`` entry.
    Returns the policy directory and an environment with both first on PATH.
    """
    native = root / "rustup first"
    native.mkdir()
    (native / "cargo").write_text('#!/bin/sh\ntouch "$BAD_CARGO_MARKER"\nexit 99\n')
    (native / "cargo").chmod(0o755)
    host = root / "qualified host"
    host.mkdir()
    policy = """#!/usr/bin/env bash
case "${0##*/}" in
  awaken-build-admission) exit 98 ;;
esac
sleep "$FIXTURE_WAIT"
[ "$FIXTURE_STATUS" = 0 ] || exit "$FIXTURE_STATUS"
export AWAKEN_CARGO_ADMITTED=1
entry=$(command -v "cargo-$1") || exit 97
exec "$entry" "$@"
"""
    for name in ("cargo", "awaken-build-admission"):
        (host / name).write_text(policy)
        (host / name).chmod(0o755)
    # Git hooks and an outer gate carry live index/receipt coordinates.
    # The fixture owns neither; never let its nested gate consume them.
    env = dict({key: value for key, value in os.environ.items()
                if not key.startswith(("GIT_", "CARGO_", "AWAKEN_"))},
               PATH=f"{native}:{host}:{os.environ['PATH']}",
               TMPDIR=str(root), BAD_CARGO_MARKER=str(root / "wrong-cargo-ran"),
               FIXTURE_WAIT="0", FIXTURE_STATUS="0", INJECTION_MARKER=str(root / "unexpected"),
               GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
    return host, env
