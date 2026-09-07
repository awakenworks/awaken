#!/usr/bin/env python3
"""Exercise the recovery drill's real admission before any Docker effect."""

import os
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


DRILL = Path(__file__).resolve().with_name("postgres_recovery_drill.sh")


class RecoveryRootTests(unittest.TestCase):
    def test_producer_allocates_and_cleans_only_its_own_directory(self):
        # P1 absolute/trailing-slash/symlink TMPDIR x failed Docker startup:
        # the real pg_tests producer allocates base+wal under that physical
        # parent and removes only this new root. An inherited caller-owned
        # AWAKEN_RECOVERY_ROOT must remain byte-for-byte untouched (P2).
        with tempfile.TemporaryDirectory(prefix="awaken-recovery-producer-test-") as tmp:
            fixture = Path(tmp).resolve()
            parent = fixture / "configured parent"
            parent.mkdir()
            alias = fixture / "parent-alias"
            alias.symlink_to(parent, target_is_directory=True)
            caller = fixture / "caller-owned"
            caller.mkdir()
            (caller / "sentinel").write_bytes(b"preserved")
            binary = fixture / "bin"
            binary.mkdir()
            report = fixture / "allocation.json"
            docker = binary / "docker"
            docker.write_text(f"#!{sys.executable}\n" + '''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
if args[0] in ("info", "rm"):
    sys.exit(0)
if args[0] == "run":
    mount = next(value for value in args if value.endswith(":/recovery-base"))
    base = Path(mount.removesuffix(":/recovery-base"))
    Path(os.environ["AWAKEN_TEST_ALLOCATION"]).write_text(json.dumps({
        "root": str(base.parent), "base": base.is_dir(),
        "wal": (base.parent / "wal").is_dir(),
    }))
sys.exit(73)
''')
            docker.chmod(0o755)
            for selected in (parent, f"{parent}///", alias):
                with self.subTest(rule="P1/P2", parent=str(selected)):
                    report.unlink(missing_ok=True)
                    result = subprocess.run(
                        ["bash", str(DRILL.with_name("pg_tests.sh")), "--require-docker"],
                        env=dict(os.environ, TMPDIR=str(selected),
                                 PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                                 CARGO_TARGET_DIR=str(fixture / "target"),
                                 AWAKEN_RECOVERY_ROOT=str(caller),
                                 AWAKEN_TEST_ALLOCATION=str(report)),
                        text=True, capture_output=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn("Docker could not create", result.stderr)
                    allocated = json.loads(report.read_text())
                    self.assertTrue(allocated["base"] and allocated["wal"])
                    path = Path(allocated["root"])
                    self.assertEqual(path.parent, parent)
                    self.assertFalse(path.exists(), "P1 cleanup after failed startup")
                    self.assertEqual((caller / "sentinel").read_bytes(), b"preserved")

    def test_configured_parent_and_exact_owned_leaf_control_admission(self):
        # Causes: C1 the selected TMPDIR (absolute, trailing slash, relative or
        # symlink alias); C2 an exact six-ASCII-alphanumeric allocation leaf;
        # C3 a direct root with direct base/wal directories. Effects: E1 admit
        # only C1+C2+C3 and propagate the first Docker failure; E2 otherwise
        # reject before writing content or invoking Docker. The fixture owns
        # every directory; no actual Docker operation runs in this unit test.
        # Decision table: R1 all C1 representations x C2+C3 => E1; R2 each
        # malformed/outside/descendant leaf => E2; R3 absent or symlinked root,
        # base or wal => E2. A six-character traversal suffix is deliberately
        # included: shell case '?' matches '/' and is not a safe leaf grammar.
        with tempfile.TemporaryDirectory(prefix="awaken-recovery-root-test-") as tmp:
            fixture = Path(tmp).resolve()
            parent = fixture / "configured parent"
            parent.mkdir()
            alias = fixture / "parent-alias"
            alias.symlink_to(parent, target_is_directory=True)
            binary = fixture / "bin"
            binary.mkdir()
            docker = binary / "docker"
            docker.write_text(
                '#!/bin/sh\nprintf "called\\n" >> "$AWAKEN_TEST_DOCKER_CALLS"\nexit 73\n'
            )
            docker.chmod(0o755)
            calls = fixture / "docker-calls"
            owned = parent / "awaken-pg-recovery.A1b2C3"
            owned.mkdir()
            for leaf in ("base", "wal"):
                (owned / leaf).mkdir()

            def invoke(root, selected_parent):
                calls.unlink(missing_ok=True)
                env = dict(
                    os.environ,
                    PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                    TMPDIR=str(selected_parent),
                    AWAKEN_RECOVERY_ROOT=str(root),
                    AWAKEN_TEST_DOCKER_CALLS=str(calls),
                )
                return subprocess.run(
                    ["bash", str(DRILL), "fixture-pg", "fixture-objects", "fixture-image"],
                    cwd=fixture, env=env, text=True, capture_output=True, timeout=10,
                )

            for selected_parent in (parent, f"{parent}/", "configured parent", alias):
                with self.subTest(rule="R1", parent=str(selected_parent)):
                    result = invoke(owned, selected_parent)
                    self.assertEqual(result.returncode, 73, result.stderr)
                    self.assertTrue(calls.exists(), "R1 admitted actual Docker boundary")
                    (owned / "content-before-target.txt").unlink()

            outside = fixture / "awaken-pg-recovery.A1b2C3"
            malformed = [
                parent / "awaken-pg-recovery.short",
                parent / "awaken-pg-recovery.toolong",
                parent / "awaken-pg-recovery.A1b2C_",
                owned / "child",
                outside,
                parent,
            ]
            for root in malformed:
                root.mkdir(exist_ok=True)
                for leaf in ("base", "wal"):
                    (root / leaf).mkdir(exist_ok=True)
            # Keep the lexical traversal in the input; every target is still
            # inside our private fixture, even if a broken guard accepts it.
            (parent / "awaken-pg-recovery.A").mkdir()
            traversal = f"{parent}/awaken-pg-recovery.A/../."
            for root in [*malformed, traversal, "", parent / "awaken-pg-recovery.Absent"]:
                with self.subTest(rule="R2", root=str(root)):
                    result = invoke(root, parent)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertFalse(calls.exists(), "R2 no Docker effects")

            for leaf in (None, "base", "wal"):
                path = owned if leaf is None else owned / leaf
                retained = path.with_name(path.name + "-retained")
                path.rename(retained)
                for symlink in (False, True):
                    if symlink:
                        path.symlink_to(retained, target_is_directory=True)
                    try:
                        with self.subTest(rule="R3", leaf=leaf, symlink=symlink):
                            result = invoke(owned, parent)
                            self.assertEqual(result.returncode, 2, result.stderr)
                            self.assertFalse(calls.exists(), "R3 no Docker effects")
                            self.assertFalse((retained / "content-before-target.txt").exists())
                    finally:
                        # A killed mutant may have crossed the boundary. Keep
                        # that assertion as the failure, never a later fixture
                        # rename error, and clean only our private fixture.
                        if path.is_symlink():
                            path.unlink()
                        (retained / "content-before-target.txt").unlink(missing_ok=True)
                        (owned / "content-before-target.txt").unlink(missing_ok=True)
                retained.rename(path)


if __name__ == "__main__":
    unittest.main()
