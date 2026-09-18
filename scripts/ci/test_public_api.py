#!/usr/bin/env python3
"""Execute the canonical API gate with deterministic tool observations."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parent
SURFACE = b"pub mod fixture\n"


class PublicApiTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-api-gate-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        scripts = self.root / "scripts/ci"
        scripts.mkdir(parents=True)
        for name in ("check_public_api.sh", "_cargo_target.sh"):
            shutil.copy2(SOURCE / name, scripts / name)
        self.script = scripts / "check_public_api.sh"
        self.snapshot = self.root / "public-api/fixture.txt"
        binary = self.root / "bin"
        binary.mkdir()
        tool = binary / "tool"
        tool.write_text(f"#!{sys.executable}\n" + '''import json, os, sys
from pathlib import Path
name, args = Path(sys.argv[0]).name, sys.argv[1:]
if name == "rustc":
    sys.exit(1 if os.environ.get("API_TEST_NO_NIGHTLY") else 0)
if name == "diff":
    if os.environ.get("API_TEST_DIFF_ERROR"):
        sys.exit(2)
    os.execv(os.environ["API_TEST_REAL_DIFF"], ["diff", *args])
if args == ["public-api", "--version"]:
    if os.environ.get("API_TEST_VERSION_QUERY_ERROR"):
        print("error: could not execute cargo-public-api", file=sys.stderr)
        sys.exit(101)
    if os.environ.get("API_TEST_VERSION_UNPARSEABLE"):
        print("cargo-public-api")
        sys.exit(0)
    print("cargo-public-api " + os.environ.get("API_TEST_VERSION", "0.52.0"))
elif args == ["metadata", "--no-deps", "--format-version", "1"]:
    if os.environ.get("API_TEST_METADATA_ERROR"):
        sys.exit(73)
    if os.environ.get("API_TEST_EMPTY_WORKSPACE"):
        print(json.dumps({"packages": []}))
        sys.exit(0)
    print(json.dumps({"packages": [{"name": "fixture"}]}))
else:
    assert args == ["+nightly-2026-07-14", "public-api", "-p", "fixture",
                    "--simplified", "--omit", "blanket-impls,auto-trait-impls,auto-derived-impls"], args
    Path(os.environ["API_TEST_COMPUTED"]).touch()
    if os.environ.get("API_TEST_COMPUTE_ERROR"):
        sys.exit(74)
    print("pub mod fixture")
''')
        tool.chmod(0o755)
        for name in ("cargo", "cargo-public-api", "rustc", "diff"):
            (binary / name).symlink_to(tool)
        self.environment = dict(
            os.environ, PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
            API_TEST_REAL_DIFF=shutil.which("diff"),
            API_TEST_COMPUTED=str(self.root / "computed"),
        )

    def invoke(self, *arguments, **conditions):
        return subprocess.run(
            ["bash", str(self.script), *arguments], cwd=self.root,
            env=dict(self.environment, **conditions), capture_output=True,
            text=True, timeout=10,
        )

    def seed(self, content):
        self.snapshot.parent.mkdir(exist_ok=True)
        self.snapshot.write_bytes(content)

    def test_check_is_read_only_and_requires_a_comparable_reviewed_baseline(self):
        # Causes: C1 baseline exists; C2 computed surface matches; C3 generation
        # succeeds; C4 diff returns a valid equal/different observation. Effects:
        # E1 only C1+C2+C3+C4 passes; E2 every other row fails; E3 check never
        # writes/creates the baseline, including a missing parent directory.
        # R1 equal; R2 drift; R3 missing file; R4 missing directory; R5 diff
        # observer error; R6 generator error. C2 is meaningful only with C1+C3.
        for rule, baseline, parent, conditions, expected in (
            ("R1", SURFACE, True, {}, 0),
            ("R2", b"reviewed old bytes", True, {}, 1),
            ("R3", None, True, {}, 1),
            ("R4", None, False, {}, 1),
            ("R5", SURFACE, True, {"API_TEST_DIFF_ERROR": "1"}, 1),
            ("R6", SURFACE, True, {"API_TEST_COMPUTE_ERROR": "1"}, 1),
        ):
            with self.subTest(rule=rule):
                self.snapshot.unlink(missing_ok=True)
                if self.snapshot.parent.exists():
                    self.snapshot.parent.rmdir()
                if parent:
                    self.snapshot.parent.mkdir()
                if baseline is not None:
                    self.seed(baseline)
                result = self.invoke("--require-tools", **conditions)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(self.snapshot.parent.exists(), parent, "E3 directory mutation")
                actual = self.snapshot.read_bytes() if self.snapshot.exists() else None
                self.assertEqual(actual, baseline, "E3 snapshot mutation")

    def test_only_explicit_bless_writes_successfully_computed_surface(self):
        # B1 missing baseline + explicit bless + successful generation => create;
        # B2 changed baseline => replace; B3 failed generation => retain exact
        # original bytes and fail, never publish empty/partial tool output.
        for rule, baseline, conditions, expected in (
            ("B1", None, {}, 0),
            ("B2", b"reviewed old bytes", {}, 0),
            ("B3", b"reviewed old bytes", {"API_TEST_COMPUTE_ERROR": "1"}, 1),
        ):
            with self.subTest(rule=rule):
                self.snapshot.unlink(missing_ok=True)
                if baseline is not None:
                    self.seed(baseline)
                result = self.invoke("--bless", "--require-tools", **conditions)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(self.snapshot.read_bytes(), SURFACE if expected == 0 else baseline)

    def test_tool_admission_is_three_outcomes_and_fails_closed_by_default(self):
        """Present / absent / undecidable, and only absence is ever waivable.

        Causes: T1 the pinned tool version is wrong; T2 the pinned nightly is not
        installed -- both are decided absences. T3 the version query itself fails;
        T4 it succeeds but prints no version -- both leave the pinned rendering
        undecided. Callers: A no flag (the default); B --require-tools (named by
        the release manifest, and now the same thing); C --allow-absent-tools.

        Effects: E1 refuse; E2 accept, saying that nothing was compared.
        Rules: (T1|T2) x (A|B) => E1. (T1|T2) x C => E2. (T3|T4) x every caller
        => E1, because --allow-absent-tools waives an absence and these are not
        absences. No row may reach generation or write a baseline, even under
        --bless.
        """

        self.seed(b"preserved")
        rows = (
            ("T1", {"API_TEST_VERSION": "0.0.0"}, True),
            ("T2", {"API_TEST_NO_NIGHTLY": "1"}, True),
            ("T3", {"API_TEST_VERSION_QUERY_ERROR": "1"}, False),
            ("T4", {"API_TEST_VERSION_UNPARSEABLE": "1"}, False),
        )
        for rule, conditions, waivable in rows:
            for caller, flags, expected in (
                ("A default", (), 1),
                ("B --require-tools", ("--require-tools",), 1),
                ("C --allow-absent-tools", ("--allow-absent-tools",), 0 if waivable else 1),
            ):
                with self.subTest(rule=rule, caller=caller):
                    result = self.invoke("--bless", *flags, **conditions)
                    self.assertEqual(
                        result.returncode, expected, result.stdout + result.stderr
                    )
                    self.assertFalse((self.root / "computed").exists())
                    self.assertEqual(self.snapshot.read_bytes(), b"preserved")
                    if expected == 0:
                        self.assertIn("NO crate's public API was compared", result.stdout)
                    elif not waivable:
                        self.assertIn("could not decide its own tooling", result.stderr)

    def test_a_run_that_compared_nothing_is_not_a_passing_run(self):
        """Z1 an empty workspace inventory yields zero comparisons => refuse.

        The loop can reach its end with nothing wrong and nothing observed: an
        empty member list produces no crate, no drift and no failure, and the
        gate used to print the same "snapshots match" line it prints after a real
        comparison. Z2 the normal single-crate workspace still passes, so this
        refuses emptiness rather than refusing success.
        """

        self.seed(SURFACE)
        empty = self.invoke("--require-tools", API_TEST_EMPTY_WORKSPACE="1")
        self.assertNotEqual(empty.returncode, 0, empty.stdout + empty.stderr)
        self.assertIn("compared 0 crates", empty.stderr)
        self.assertFalse((self.root / "computed").exists())
        populated = self.invoke("--require-tools")
        self.assertEqual(populated.returncode, 0, populated.stdout + populated.stderr)
        self.assertIn("1 crates compared", populated.stdout)

    def test_failed_inventory_cannot_become_an_empty_successful_workspace(self):
        # I1 Cargo metadata fails before any crate can be enumerated => nonzero,
        # zero rustdoc calls and unchanged reviewed content (pipefail boundary).
        self.seed(SURFACE)
        result = self.invoke("--require-tools", API_TEST_METADATA_ERROR="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "computed").exists())
        self.assertEqual(self.snapshot.read_bytes(), SURFACE)


if __name__ == "__main__":
    unittest.main()
