#!/usr/bin/env python3
"""Install or verify the standalone main guard in a Git receiving repository.

receive-pack runs reference-transaction from GIT_DIR, including when main is in
a linked worktree. A Lefthook launcher cannot assume it can discover a checkout
configuration there. Install the existing stdlib-only guard itself: the policy
and its stdin protocol stay identical, without a checkout or Lefthook dependency.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


GUARD = Path(__file__).with_name("check_main_linear.py")


def git_environment() -> dict[str, str]:
    # In particular, a caller's GIT_DIR must not override --repository.
    return {**{key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0"}


def hook_path(repository: Path) -> Path:
    result = subprocess.run(
        ["git", "-C", str(repository), "rev-parse", "--path-format=absolute",
         "--git-path", "hooks/reference-transaction"],
        env=git_environment(), capture_output=True, text=True, check=True,
    )
    return Path(result.stdout.strip()).resolve()


def configure(repository: Path, *, install: bool, source: Path = GUARD) -> dict[str, object]:
    destination = hook_path(repository)
    # Git for Windows may check out this source with CRLF; an installed Linux
    # shebang must not become `python3\r` when an operator transfers that file.
    content = source.read_bytes().replace(b"\r\n", b"\n")
    current = destination.read_bytes() if destination.is_file() else None
    digest = hashlib.sha256(content).hexdigest()
    backup = None
    if install:
        destination.parent.mkdir(parents=True, exist_ok=True)
        if current is not None and current != content:
            backup = destination.parent / "awaken-backups" / (
                "reference-transaction-" + hashlib.sha256(current).hexdigest())
            backup.parent.mkdir(exist_ok=True)
            if backup.exists():
                if backup.read_bytes() != current:
                    raise RuntimeError("receive hook backup content does not match its hash")
            else:
                with backup.open("xb") as stream:
                    stream.write(current)
        if current != content:
            descriptor, temporary = tempfile.mkstemp(prefix=".awaken-main-", dir=destination.parent)
            try:
                with os.fdopen(descriptor, "wb") as stream:
                    stream.write(content)
                os.chmod(temporary, 0o755)
                os.replace(temporary, destination)
            finally:
                Path(temporary).unlink(missing_ok=True)
        destination.chmod(destination.stat().st_mode | 0o111)
    matches = destination.is_file() and destination.read_bytes() == content
    executable = destination.is_file() and os.access(destination, os.X_OK)
    return {"repository": str(repository.resolve()), "hook": str(destination),
            "sha256": digest, "matches": matches, "executable": executable,
            "installed": install, "backup": str(backup) if backup else None,
            "passed": matches and executable}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--install", action="store_true")
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--self-test", action="store_true")
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    args = parser.parse_args(argv)
    if args.self_test:
        import unittest
        from _receive_hook_tests import ReceiveHookTests
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(ReceiveHookTests)).wasSuccessful() else 1
    try:
        result = configure(args.repository, install=args.install)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Receive hook configuration FAILED: {error}")
        return 1
    print(json.dumps(result, ensure_ascii=False))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
