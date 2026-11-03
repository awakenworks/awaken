"""Failure-first design for exact-revision and cold-environment verification."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import check_dependency_fetch as fetch


class DependencyFetchTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="dependency fetch ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()

    def test_isolation_removes_offline_rewrites_and_injected_config(self):
        with patch.dict(os.environ, {"CARGO_NET_OFFLINE": "true", "CARGO_HOME": "warm",
                                    "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "url.private.insteadOf",
                                    "GIT_CONFIG_VALUE_0": "https://github.com/", "GIT_DIR": "other"}):
            env = fetch.clean_environment()
        self.assertNotIn("CARGO_HOME", env)
        self.assertNotIn("CARGO_NET_OFFLINE", env)
        self.assertNotIn("GIT_CONFIG_COUNT", env)
        self.assertNotIn("GIT_DIR", env)
        self.assertEqual(env["GIT_CONFIG_GLOBAL"], os.devnull)
        self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")

    def test_ci_cannot_replace_cold_fetch_with_a_warm_or_git_only_tier(self):
        # Cause/effect graph: C1=cold admission is unconditional and uncached;
        # C2=only terminal observations use their run-bound always condition;
        # C3=the evidence upload is always retained. E1=admit iff C1+C2+C3;
        # E2=reject a condition on admission, cache/bypass, or a terminal step
        # hiding verification. Decision rows: shipped workflow -> E1; each
        # single invalid cause below -> E2. Telemetry must not grant admission.
        workflow = (fetch.ROOT / ".github/workflows/quality.yml").read_text(encoding="utf-8")
        self.assertEqual(fetch.workflow_errors(workflow), [])
        admission = "        run: python scripts/ci/check_dependency_fetch.py --output target/test-timings/dependency-fetch"
        terminal_condition = "        if: always() && env.AWAKEN_BUILD_RUN_ID != ''\n"
        for name in ("Finish build observations", "Summarize build observations"):
            prefix = f"      - name: {name}\n"
            self.assertIn(prefix + terminal_condition, workflow)
            for changed_step in (
                prefix,
                prefix + "        if: false\n",
                prefix + "        if: always()\n",
            ):
                changed = workflow.replace(prefix + terminal_condition, changed_step, 1)
                with self.subTest(terminal=name, mutation=changed_step):
                    self.assertTrue(fetch.workflow_errors(changed))
            hidden = workflow.replace(admission, "        run: echo bypassed admission", 1).replace(
                prefix + terminal_condition, prefix + terminal_condition + admission + "\n", 1)
            with self.subTest(hidden_admission=name):
                self.assertTrue(fetch.workflow_errors(hidden))
        for condition in ("always()", "false", "always() && env.AWAKEN_BUILD_RUN_ID != ''"):
            with self.subTest(admission_condition=condition):
                self.assertTrue(fetch.workflow_errors(workflow.replace(
                    admission, f"        if: {condition}\n" + admission, 1)))
        for changed in (workflow.replace("dependency_fetch:", "other:", 1),
                        workflow.replace("needs: dependency_fetch", "needs: other", 1),
                        workflow.replace("runner: [ubuntu-22.04, windows-2025]", "runner: [ubuntu-22.04]", 1),
                        workflow.replace("--output target/test-timings/dependency-fetch", "--git-only --output target/test-timings/dependency-fetch", 1),
                        workflow.replace("    # A warm Cargo", "    # rust-cache\n    # A warm Cargo", 1),
                        workflow.replace("  dependency_fetch:\n", "  dependency_fetch:\n    if: false\n", 1),
                        workflow.replace("          python scripts/ci/check_trust_roots.py --self-test\n", "", 1),
                        workflow.replace("          python scripts/ci/check_trust_roots.py\n", "", 1)):
            self.assertTrue(fetch.workflow_errors(changed))
        setup = ('      - uses: actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1 # v6\n'
                 '        with:\n          python-version: "3.12"\n')
        for changed in (workflow.replace(setup, "", 1),
                        workflow.replace('python-version: "3.12"', 'python-version: "3.10"', 1),
                        workflow.replace('setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1',
                                         'setup-python@v6', 1),
                        workflow.replace(setup, "", 1).replace(
                            "      - name: Retain cold dependency evidence", setup +
                            "      - name: Retain cold dependency evidence", 1)):
            self.assertTrue(fetch.workflow_errors(changed))

    def test_every_quality_job_bootstraps_python_before_running_checkers(self):
        # P1 all jobs bootstrap => admit. P2 missing/old/floating/conditional or
        # late setup in any one job => reject, even if every other job is valid.
        import re
        workflow = (fetch.ROOT / ".github/workflows/quality.yml").read_text(encoding="utf-8")
        setup = ('      - uses: actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1 # v6\n'
                 '        with:\n          python-version: "3.12"\n')
        self.assertEqual(fetch.workflow_errors(workflow), [])
        for job in ("dependency_fetch", "trusted_host_portability", "full_quality", "performance_summary"):
            match = re.search(r"(?ms)^  " + job + r":\n(.*?)(?=^  \S|\Z)", workflow)
            body = match.group(0)
            self.assertEqual(body.count(setup), 1)
            for bad in (body.replace(setup, ""),
                        body.replace('python-version: "3.12"', 'python-version: "3.10"'),
                        body.replace('setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1', 'setup-python@v6'),
                        body.replace('        with:\n          python-version:', '        if: false\n        with:\n          python-version:'),
                        body.replace('        with:\n          python-version:', '        continue-on-error: true\n        with:\n          python-version:'),
                        body.replace(setup, "").rstrip() + "\n" + setup):
                with self.subTest(job=job, mutation=bad):
                    errors = fetch.workflow_errors(workflow[:match.start()] + bad + workflow[match.end():])
                    self.assertIn(job + " must bootstrap the pinned Python before its first checker", errors)

    def test_native_ci_refuses_warm_unpublished_dependency_regression(self):
        workflow = (fetch.ROOT / ".github/workflows/windows-native-services.yml").read_text(encoding="utf-8")
        self.assertEqual(fetch.native_workflow_errors(workflow), [])
        admission = (
            "      - name: Require publicly reachable locked dependencies from empty stores\n"
            "        run: python scripts/ci/check_dependency_fetch.py --output target/test-timings/native-dependency-fetch\n")
        self.assertEqual(workflow.count(admission), 1)
        cases = [workflow.replace("  native_services:\n", "  native_services:\n    if: false\n"), workflow.replace(admission, ""), workflow.replace(admission, admission + admission),
                 workflow.replace("--output target/test-timings/native-dependency-fetch", "--git-only --output target/test-timings/native-dependency-fetch"),
                 workflow.replace(admission, admission.replace("        run:", "        if: false\n        run:")),
                 workflow.replace(admission, admission.replace("        run:", "        continue-on-error: true\n        run:")),
                 workflow.replace("runs-on: windows-2025", "runs-on: ubuntu-22.04"),
                 workflow.replace(admission, "      - uses: actions/cache@fixture\n" + admission),
                 workflow.replace(admission, "").replace("      - name: Retain exact-source", admission + "      - name: Retain exact-source")]
        for changed in cases:
            with self.subTest(mutation=changed):
                self.assertTrue(fetch.native_workflow_errors(changed))

    def test_inventory_deduplicates_and_rejects_invalid_sources(self):
        revision = "1" * 40
        manifest = self.root / "Cargo.toml"
        manifest.write_text('[dependencies]\nx={git="https://github.com/awakenworks/awaken-iam.git",rev="' + revision + '"}\n')
        lock = self.root / "Cargo.lock"
        lock.write_text('[[package]]\nname="x"\nsource="git+https://github.com/awakenworks/awaken-iam.git?rev=' + revision + '#' + revision + '"\n')
        self.assertEqual(fetch.git_inputs(self.root, ["Cargo.toml", "Cargo.lock"]),
                         [("https://github.com/awakenworks/awaken-iam", revision)])
        for bad in ('ssh://host/private', 'https://example.invalid/private'):
            manifest.write_text('[dependencies]\nx={git="' + bad + '",rev="' + revision + '"}\n')
            with self.assertRaises(ValueError):
                fetch.git_inputs(self.root, ["Cargo.toml"])
        manifest.write_text('[dependencies]\nx={git="https://github.com/awakenworks/awaken-iam",branch="main"}\n')
        with self.assertRaisesRegex(ValueError, "full 40-character rev"):
            fetch.git_inputs(self.root, ["Cargo.toml"])
        lock.write_text(lock.read_text().replace('#' + revision, '#' + '2' * 40))
        with self.assertRaisesRegex(ValueError, "does not match"):
            fetch.git_inputs(self.root, ["Cargo.lock"])

    def test_empty_inventory_is_not_a_successful_skip(self):
        with self.assertRaisesRegex(ValueError, "No tracked Git dependency"):
            fetch.git_inputs(self.root, [])

    def test_real_publication_fetch_is_independent_of_a_warm_store(self):
        env = fetch.clean_environment()
        repository = self.root / "upstream"
        subprocess.run(["git", "init", "--initial-branch=main", str(repository)], env=env,
                       check=True, capture_output=True)
        def git(*args):
            return subprocess.check_output(["git", "-C", str(repository), *args], env=env).decode().strip()
        git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "public")
        revision = git("rev-parse", "HEAD")
        def verify(label, rev):
            logs = self.root / (label + "-logs")
            logs.mkdir()
            return fetch.fetch_revision(repository.as_uri(), rev, self.root / label, logs, env)
        first = verify("warm", revision)
        self.assertTrue(first["passed"], first)
        self.assertEqual(first["resolved_revision"], revision)
        private = self.root / "private"
        subprocess.run(["git", "init", str(private)], env=env, check=True, capture_output=True)
        subprocess.run(["git", "-C", str(private), "-c", "user.name=fixture",
                        "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                        "commit", "--allow-empty", "-m", "unpublished"], env=env, check=True, capture_output=True)
        unpublished = subprocess.check_output(["git", "-C", str(private), "rev-parse", "HEAD"], env=env).decode().strip()
        subprocess.run(["git", "--git-dir", str(self.root / "warm"), "fetch", private.as_uri(), unpublished],
                       env=env, check=True, capture_output=True)
        subprocess.run(["git", "--git-dir", str(self.root / "warm"), "cat-file", "-e", unpublished + "^{commit}"],
                       env=env, check=True, capture_output=True)
        # The exact object exists in a warm cache, but the public source lacks it.
        missing = verify("cold-missing", unpublished)
        self.assertFalse(missing["passed"])
        self.assertNotEqual(missing["stages"][-1]["exit_code"], 0)
        self.assertTrue(verify("cold-valid", revision)["passed"])

    def test_timeout_and_wrong_resolved_object_fail_closed(self):
        with patch.object(fetch.subprocess, "run", side_effect=subprocess.TimeoutExpired("git", 1)):
            row = fetch.run(["git"], self.root, fetch.clean_environment(), self.root / "timeout.log")
        self.assertEqual(row["exit_code"], 124)
        logs = self.root / "wrong"
        logs.mkdir()
        def successful_wrong(argv, cwd, environment, log, **kwargs):
            log.write_text("2" * 40 if "rev-parse" in argv else "")
            return {"exit_code": 0}
        with patch.object(fetch, "run", side_effect=successful_wrong):
            result = fetch.fetch_revision("public", "1" * 40, self.root / "wrong-store", logs, {})
        self.assertFalse(result["passed"])

    def test_one_unavailable_revision_blocks_snapshot_and_cargo_even_after_a_valid_probe(self):
        output = self.root / "unavailable-evidence"
        with patch.object(fetch, "tracked_paths", return_value=[]), \
             patch.object(fetch, "source_identity", return_value={"revision": "fixture"}), \
             patch.object(fetch, "git_inputs", return_value=[("one", "1" * 40), ("two", "2" * 40)]), \
             patch.object(fetch, "fetch_revision", side_effect=[{"passed": True}, {"passed": False}]), \
             patch.object(fetch, "snapshot") as snapshot, patch.object(fetch, "run") as cargo:
            result = fetch.verify(self.root, output)
        self.assertFalse(result["passed"])
        self.assertIsNone(result["cargo_fetch"])
        snapshot.assert_not_called()
        cargo.assert_not_called()
        saved = json.loads((output / "result.json").read_text())
        self.assertEqual([row["passed"] for row in saved["git_dependencies"]], [True, False])
        self.assertFalse(saved["passed"])

    def test_cargo_failure_is_retained_and_cannot_turn_green(self):
        output = self.root / "evidence"
        with patch.object(fetch, "tracked_paths", return_value=[]), \
             patch.object(fetch, "source_identity", return_value={"revision": "fixture"}), \
             patch.object(fetch, "git_inputs", return_value=[("public", "1" * 40)]), \
             patch.object(fetch, "fetch_revision", return_value={"passed": True}), \
             patch.object(fetch, "snapshot", return_value="snapshot"), \
             patch.object(fetch, "run", side_effect=[{"exit_code": 0}, {"exit_code": 101}]):
            result = fetch.verify(self.root, output)
        self.assertFalse(result["passed"])
        saved = json.loads((output / "result.json").read_text())
        self.assertEqual(saved["cargo_fetch"]["exit_code"], 101)
        self.assertEqual(saved["cargo_home_initial_entries"], [])
