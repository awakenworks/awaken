"""Real Git receive tests; no production repositories, hooks or global config."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from install_receive_hooks import configure, git_environment, hook_path


class ReceiveHookTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="receive hook ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.environment = {**git_environment(), "GIT_CONFIG_GLOBAL": os.devnull,
                            "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0"}
        self.repository = self.root / "server.git"
        self.git(self.root, "init", "--bare", "--initial-branch=main", str(self.repository))
        # Keep main out of HEAD so deletion reaches our guard, rather than Git's
        # separate denyDeleteCurrent policy refusing it first.
        self.git(self.repository, "symbolic-ref", "HEAD", "refs/heads/parking")
        self.assertTrue(configure(self.repository, install=True)["passed"])
        self.client = self.root / "client"
        self.git(self.root, "init", "--initial-branch=main", str(self.client))
        self.git(self.client, "config", "user.name", "Receive fixture")
        self.git(self.client, "config", "user.email", "receive@example.invalid")
        self.git(self.client, "config", "commit.gpgsign", "false")
        self.git(self.client, "remote", "add", "origin", str(self.repository))
        self.first = self.commit("first")

    def git(self, cwd, *args, check=True):
        result = subprocess.run(["git", "-C", str(cwd), *args], env=self.environment,
                                capture_output=True, text=True, timeout=30)
        if check:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def commit(self, value):
        (self.client / "value").write_text(value, encoding="utf-8")
        self.git(self.client, "add", "value")
        self.git(self.client, "commit", "--quiet", "-m", value)
        return self.git(self.client, "rev-parse", "HEAD").stdout.strip()

    def main(self):
        return self.git(self.repository, "rev-parse", "main").stdout.strip()

    def rejected(self, *args, diagnostic):
        before = self.main()
        result = self.git(self.client, "push", *args, check=False)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(diagnostic, result.stderr)
        self.assertEqual(self.main(), before)

    def test_bare_receive_creation_fast_forward_force_rewind_and_deletion(self):
        self.git(self.client, "push", "origin", "HEAD:main")
        self.assertEqual(self.main(), self.first)
        second = self.commit("second")
        self.git(self.client, "push", "origin", "HEAD:main")
        self.assertEqual(self.main(), second)
        self.rejected("--force", "origin", self.first + ":main", diagnostic="not a fast-forward")
        self.rejected("origin", ":main", diagnostic="main may not be deleted")
        self.git(self.client, "push", "origin", "HEAD:feature")
        self.git(self.repository, "pack-refs", "--all", "--prune")
        self.assertEqual(self.main(), second)

    def test_fast_forward_that_introduces_merge_is_refused(self):
        self.git(self.client, "push", "origin", "HEAD:main")
        self.commit("main second")
        self.git(self.client, "push", "origin", "HEAD:main")
        self.git(self.client, "switch", "-c", "side", self.first)
        (self.client / "side").write_text("side", encoding="utf-8")
        self.git(self.client, "add", "side")
        self.git(self.client, "commit", "--quiet", "-m", "side")
        self.git(self.client, "switch", "main")
        self.git(self.client, "merge", "--no-ff", "side", "-m", "merge")
        self.rejected("origin", "HEAD:main", diagnostic="adds merge commit")

    def test_linked_main_clean_update_and_dirty_worktree_refusal(self):
        server = self.root / "nonbare"
        self.git(self.root, "clone", str(self.client), str(server))
        self.git(server, "switch", "-c", "parking")
        linked = self.root / "linked main"
        self.git(server, "worktree", "add", str(linked), "main")
        self.assertEqual(hook_path(server), hook_path(linked))
        self.assertTrue(configure(linked, install=True)["passed"])
        self.git(server, "config", "receive.denyCurrentBranch", "updateInstead")
        self.git(self.client, "remote", "set-url", "origin", str(server))
        second = self.commit("second")
        result = self.git(self.client, "push", "origin", "HEAD:main")
        self.assertNotIn("No config files", result.stderr)
        self.assertEqual(self.git(linked, "rev-parse", "HEAD").stdout.strip(), second)
        self.assertEqual(self.git(linked, "status", "--porcelain=v1").stdout, "")
        self.commit("third")
        for staged in (False, True):
            (linked / "value").write_text("owned change", encoding="utf-8")
            if staged:
                self.git(linked, "add", "value")
            result = self.git(self.client, "push", "origin", "HEAD:main", check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.git(server, "rev-parse", "main").stdout.strip(), second)
            self.assertEqual((linked / "value").read_text(), "owned change")

    def test_install_check_drift_backup_and_idempotence(self):
        path = hook_path(self.repository)
        path.write_bytes(b"#!/bin/sh\nexit 0\n")
        self.assertFalse(configure(self.repository, install=False)["passed"])
        installed = configure(self.repository, install=True)
        self.assertTrue(installed["passed"])
        self.assertEqual(Path(installed["backup"]).read_bytes(), b"#!/bin/sh\nexit 0\n")
        self.assertIsNone(configure(self.repository, install=True)["backup"])
        self.assertTrue(configure(self.repository, install=False)["passed"])

    def test_receive_guard_does_not_need_worktree_configuration(self):
        self.git(self.client, "push", "origin", "HEAD:main")
        second = self.commit("second")
        self.git(self.client, "push", "origin", "HEAD:feature")
        self.git(self.repository, "update-ref", "refs/heads/main", second, self.first)
        result = self.git(self.repository, "update-ref", "refs/heads/main", self.first, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a fast-forward", result.stderr)
        self.assertEqual(self.main(), second)

    def test_installer_normalizes_cross_platform_shebang(self):
        from install_receive_hooks import GUARD
        windows_source = self.root / "guard-with-crlf.py"
        windows_source.write_bytes(GUARD.read_bytes().replace(b"\r\n", b"\n").replace(b"\n", b"\r\n"))
        self.assertTrue(configure(self.repository, install=True, source=windows_source)["passed"])
        self.assertNotIn(b"\r", hook_path(self.repository).read_bytes())
        self.git(self.client, "push", "origin", "HEAD:main")
        self.commit("second")
        self.git(self.client, "push", "origin", "HEAD:main")
        self.rejected("--force", "origin", self.first + ":main", diagnostic="not a fast-forward")
