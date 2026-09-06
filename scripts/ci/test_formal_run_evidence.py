"""Execution-receipt decision tables; no fixture claims to prove Rust code."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

from formal_run_evidence import ROOT, begin_record, finish_record, validate_record, write_record


def prepare_source_fixture(root: Path, extra_sources: dict[str, str] | None = None) -> None:
    """Shared synthetic Git/receipt fixture; it never attests a real proof run."""
    root.mkdir()
    for relative in ("scripts/ci/formal_run_evidence.py", "scripts/ci/check_formal.sh",
                     "scripts/ci/bootstrap_kani.sh", "scripts/ci/_cargo_target.sh", "rust-toolchain.toml"):
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    for relative, source in {".gitignore": "target/\n", "transitive.rs": "// dependency\n",
                             **(extra_sources or {})}.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source)
    for arguments in (("init", "--quiet"), ("config", "core.autocrlf", "false"), ("add", "."),
                      ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
        subprocess.run(["git", *arguments], cwd=root, check=True, capture_output=True)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-proof-receipt-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo"
        prepare_source_fixture(self.root)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.STDOUT)

    def test_completion_requires_strict_unchanged_inputs_and_all_tools(self):
        # R1 strict+unchanged+all tools succeeds; R2 optional/R3 missing tools/
        # R4 source changes/R5 time reverses/R6 second completion all reject.
        record = begin_record(self.root, True, 100)
        passed = finish_record(record, self.root, False, 110)
        validate_record(passed, self.root, 110)
        for changed, missing, now in (
            (dict(record, strict=False), False, 110), (record, True, 110),
            (record, False, 99), (passed, False, 111),
        ):
            with self.subTest(missing=missing, now=now, status=changed["status"]):
                with self.assertRaises(ValueError):
                    finish_record(changed, self.root, missing, now)
        (self.root / "transitive.rs").write_text("// changed dependency\n")
        with self.assertRaisesRegex(ValueError, "source changed"):
            finish_record(record, self.root, False, 110)

    def test_every_release_identity_axis_is_binding(self):
        # V2-V4: take a valid completed receipt, change one axis at a time.
        # Every mutation rejects independently; the positive row prevents a
        # reject-everything implementation from satisfying this decision table.
        passed = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        validate_record(passed, self.root, 110)
        mutations = (
            ("status", "running"), ("strict", False), ("schema_version", 0),
            ("run_id", ""), ("started_at_unix", 120), ("finished_at_unix", 111),
            ("source", dict(passed["source"], commit="a" * 40)),
            ("source", dict(passed["source"], tree="a" * 40)),
            ("source", dict(passed["source"], clean=False)),
            ("proof_toolchain", {}), ("runner", dict(passed["runner"], digest="sha256:" + "0" * 64)),
        )
        for key, value in mutations:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                validate_record(dict(passed, **{key: value}), self.root, 110)

    def test_new_invocation_retires_old_success_and_atomic_file_is_valid(self):
        # C1 previous Passed; C2 new Running. E1 the same output path no longer
        # validates until the new invocation completes. No second report store.
        path = self.root / "target/formal-run.json"
        first = begin_record(self.root, True, 100)
        write_record(path, finish_record(first, self.root, False, 110))
        validate_record(json.loads(path.read_text()), self.root, 110)
        second = begin_record(self.root, True, 120)
        self.assertNotEqual(first["run_id"], second["run_id"])
        write_record(path, second)
        with self.assertRaises(ValueError):
            validate_record(json.loads(path.read_text()), self.root, 120)
        self.assertEqual([p.name for p in path.parent.iterdir()], [path.name])

    def test_actual_gate_failure_leaves_running_not_stale_success(self):
        # C1 a completed previous run; C2 the actual check_formal.sh starts;
        # C3 its first checker fails with 23. R=C1+C2+C3 => nonzero body and
        # Running receipt, with no Cargo/proof call and no stale green reuse.
        path = self.root / "target/test-timings/formal-run.json"
        write_record(path, finish_record(begin_record(self.root, True, 100), self.root, False, 110))
        bindir = Path(self.temporary.name) / "bin"
        bindir.mkdir()
        shim = bindir / "python3"
        shim.write_text("#!/bin/sh\ncase \"$1\" in\n"
                        "  scripts/ci/formal_run_evidence.py) exec " + shlex.quote(sys.executable) + " \"$@\" ;;\n"
                        "  *) exit 23 ;;\nesac\n")
        shim.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                   AWAKEN_FORMAL_RUN_EVIDENCE=str(path))
        result = subprocess.run(["bash", "scripts/ci/check_formal.sh", "--require-tools"],
                                cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(json.loads(path.read_text())["status"], "running")

    def test_dirty_diagnostic_run_never_qualifies_release(self):
        # A stable dirty tree may complete for local diagnosis; it cannot
        # attest a release commit, including newly added/untracked Rust input.
        (self.root / "untracked.rs").write_text("// new module\n")
        record = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(record, self.root, 110)

    def test_git_normalized_windows_checkout_preserves_source_identity(self):
        # E1 a clean LF checkout completes; E2 Windows-style autocrlf checkout
        # changes physical bytes but not Git source. The same receipt validates.
        # E3 a semantic edit after conversion still rejects, so normalization
        # cannot conceal a changed runner or weaken the clean-source boundary.
        runner = self.root / "scripts/ci/check_formal.sh"
        self.git("config", "core.autocrlf", "false")
        self.assertFalse(b"\r\n" in runner.read_bytes(), "LF fixture")
        passed = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        windows = self.root.parent / "windows-checkout"
        subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", "--config", "core.autocrlf=true",
                        str(self.root), str(windows)], check=True, capture_output=True)
        runner = windows / "scripts/ci/check_formal.sh"
        self.assertTrue(b"\r\n" in runner.read_bytes(), "CRLF fixture")
        validate_record(passed, windows, 110)
        runner.write_bytes(runner.read_bytes() + b"exit 23\r\n")
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(passed, windows, 110)


if __name__ == "__main__":
    unittest.main()
