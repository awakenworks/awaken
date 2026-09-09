"""Bootstrap publication rules; compiler semantics are checked by the real Kani fixture."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

# Instrument external build tools only. The real bootstrap owns all ordering,
# digest comparisons, cache selection and publication; no shell body is copied.
TOOL = r'''#!/usr/bin/env python3
import os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(name + " " + " ".join(args) + "\n")
if name == "git":
    source = None
    if args[:1] == ["-C"]:
        source = pathlib.Path(args[1]); args = args[2:]
    action = args[0]
    if action == "clone":
        source = pathlib.Path(args[-1]); (source / ".git").mkdir(parents=True)
        (source / "scripts").mkdir()
        (source / "scripts/kani").symlink_to(os.environ["KANI_WITNESS"])
    elif action == "rev-parse":
        print(os.environ.get("ACTUAL_REVISION", os.environ["PINNED_REVISION"]))
    elif action == "ls-files":
        print("unexpected.rs" if os.environ.get("UNTRACKED") else "")
    elif action == "apply" and "--check" not in args:
        (source / "applied").touch()
    elif action == "diff":
        if "--cached" in args:
            sys.exit(int(os.environ.get("STAGED", "0")))
        if "--quiet" in args:
            sys.exit(int((source / "applied").exists() or bool(os.environ.get("DRIFT"))))
        if (source / "applied").exists():
            sys.stdout.write(pathlib.Path(os.environ["PATCH"]).read_text())
        if os.environ.get("DRIFT"):
            print("unreviewed change")
elif name == "cargo":
    build = pathlib.Path.cwd() / "target/kani"
    build.mkdir(parents=True, exist_ok=True)
    (build / "compiler").write_text("compiled bytes")
    sys.exit(int(os.environ.get("BUILD_EXIT", "0")))
elif name == "kani":
    sys.exit(int(os.environ.get("REGRESSION_EXIT", "0")))
'''


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-kani-bootstrap-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.scripts = self.root / "scripts/ci"
        for relative in ("bootstrap_kani.sh", "patches/kani-wide-niche.patch",
                         "fixtures/kani_wide_niche.rs"):
            target = self.scripts / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / "scripts/ci" / relative, target)
        source = (self.scripts / "bootstrap_kani.sh").read_text()
        revision = re.search(r'readonly kani_revision="([^"]+)"', source)[1]
        digest = re.search(r'readonly kani_patch_sha256="([^"]+)"', source)[1]
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.previous = self.cache / "previous"
        self.previous.mkdir()
        (self.cache / "current").symlink_to(self.previous)
        self.patched = self.cache / f"{revision}-{digest[:12]}"
        bindir = self.root / "bin"
        bindir.mkdir()
        for name in ("git", "cargo", "rustup", "kani", "cbmc", "goto-cc"):
            tool = bindir / name
            tool.write_text(TOOL)
            tool.chmod(0o755)
        self.calls = self.root / "calls"
        self.env = dict(os.environ, PATH=f"{bindir}:{os.environ['PATH']}",
                        AWAKEN_KANI_CACHE_DIR=str(self.cache),
                        AWAKEN_KANI_BACKEND_DIR=str(bindir), CALLS=str(self.calls),
                        PINNED_REVISION=revision, PATCH=str(self.scripts / "patches/kani-wide-niche.patch"),
                        KANI_WITNESS=str(bindir / "kani"))

    def run_bootstrap(self, **changes):
        return subprocess.run(["bash", str(self.scripts / "bootstrap_kani.sh")],
                              env=dict(self.env, **changes), capture_output=True, text=True)

    def test_success_and_exact_cache_replay_publish_only_after_qualification(self):
        # C1 clean/new or exactly patched cache; C2 successful build; C3 real
        # regression returns success. R1 C1+C2+C3 -> publish patched cache.
        # Repeated bootstrap must reuse that patch and preserve the old cache.
        for _ in range(2):
            result = self.run_bootstrap()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((self.cache / "current").resolve(), self.patched)
            self.assertTrue(self.previous.is_dir())
        calls = self.calls.read_text().splitlines()
        applied = [line for line in calls if " apply " in line and "--check" not in line]
        self.assertEqual(len(applied), 1)
        builds = [i for i, line in enumerate(calls) if line == "cargo build-dev"]
        checks = [i for i, line in enumerate(calls) if line.startswith("kani ")]
        self.assertEqual(len(checks), 2)
        self.assertEqual(len(builds), 1, "qualified cache must never rebuild in place")
        self.assertTrue(all(builds[0] < check for check in checks))
        self.assertTrue(all("--target-dir" in calls[i] and "kani_wide_niche.rs" in calls[i] for i in checks))

    def test_every_unqualified_cache_preserves_the_previously_selected_compiler(self):
        # R2 bad revision/staged/untracked/tracked drift -> reject before build.
        # R3 build failure -> no regression/publication. R4 regression failure
        # -> no publication. Failure is not permission to replace the old tool.
        for changes, reaches_build, reaches_check in (
            ({"ACTUAL_REVISION": "wrong"}, False, False),
            ({"STAGED": "1"}, False, False),
            ({"UNTRACKED": "1"}, False, False),
            ({"DRIFT": "1"}, False, False),
            ({"BUILD_EXIT": "23"}, True, False),
            ({"REGRESSION_EXIT": "24"}, True, True),
        ):
            with self.subTest(changes=changes):
                self.calls.write_text("")
                result = self.run_bootstrap(**changes)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((self.cache / "current").resolve(), self.previous)
                calls = self.calls.read_text()
                self.assertEqual("cargo build-dev" in calls, reaches_build)
                self.assertEqual(any(line.startswith("kani ") for line in calls.splitlines()), reaches_check)

    def test_changed_patch_bytes_fail_before_any_build_tool_runs(self):
        # R5 tampered patch bytes, even with a valid source revision, cannot
        # enter the toolchain. The bootstrap's pin is independent of Git output.
        patch = self.scripts / "patches/kani-wide-niche.patch"
        patch.write_text(patch.read_text() + "\n")
        result = self.run_bootstrap()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("patch digest", result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertEqual((self.cache / "current").resolve(), self.previous)

    def test_published_build_and_readonly_verification_reject_every_later_drift(self):
        # R6 published cache + repeat/failing regression -> never rebuild it.
        # R7 source revision/patch or binary drift -> reject even if the compiler
        # would pass the fixture; verifying a selected tool is a read-only path.
        self.assertEqual(self.run_bootstrap().returncode, 0)
        compiler = self.patched / "target/kani/compiler"
        before = compiler.read_bytes()
        receipt = self.patched / "target/awaken-kani-build.sha256"
        receipt_before = (receipt.read_bytes(), receipt.stat().st_mtime_ns)
        self.assertEqual(self.run_bootstrap().returncode, 0)
        self.assertEqual((receipt.read_bytes(), receipt.stat().st_mtime_ns), receipt_before)
        self.calls.write_text("")
        failed = self.run_bootstrap(REGRESSION_EXIT="24")
        self.assertEqual(failed.returncode, 24)
        self.assertEqual(compiler.read_bytes(), before)
        self.assertNotIn("cargo ", self.calls.read_text())
        self.assertEqual((self.cache / "current").resolve(), self.patched)
        for changes, succeeds in (({}, True), ({"ACTUAL_REVISION": "wrong"}, False),
                                  ({"DRIFT": "1"}, False)):
            with self.subTest(changes=changes):
                self.calls.write_text("")
                result = subprocess.run(["bash", str(self.scripts / "bootstrap_kani.sh"),
                                         "--verify-source", str(self.patched)],
                                        env=dict(self.env, **changes), capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                self.assertNotIn("cargo ", self.calls.read_text())
                self.assertNotIn("kani ", self.calls.read_text())
        compiler.write_text("changed compiler bytes")
        result = self.run_bootstrap()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("qualified cache", result.stderr)
        self.assertEqual(compiler.read_text(), "changed compiler bytes")

    def test_formal_invocation_freezes_the_selected_compiler_before_current_changes(self):
        # R8 current initially points at A, then another bootstrap publishes B.
        # All later calls in this invocation must still execute and attest A.
        for name in ("A", "B"):
            scripts = self.cache / name / "scripts"
            scripts.mkdir(parents=True)
            tool = scripts / "cargo-kani"
            tool.write_text(f"#!/bin/sh\nprintf '%s\\n' {name}\n")
            tool.chmod(0o755)
        current = self.cache / "selected"
        current.symlink_to(self.cache / "A")
        gate = (ROOT / "scripts/ci/check_formal.sh").read_text()
        selection = "kani_cache_root=" + gate.split("kani_cache_root=", 1)[1].split("\nrequire_tools=", 1)[0]
        script = selection + '\nln -sfn "$CACHE_B" "$AWAKEN_KANI_SOURCE_DIR"\nprintf "%s\\n" "$kani_source_dir"\ncargo-kani\n'
        result = subprocess.run(["bash", "-c", script], capture_output=True, text=True,
                                env=dict(self.env, AWAKEN_KANI_SOURCE_DIR=str(current),
                                         CACHE_B=str(self.cache / "B")))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [str(self.cache / "A"), "A"])


if __name__ == "__main__":
    unittest.main()
