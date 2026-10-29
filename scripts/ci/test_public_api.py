#!/usr/bin/env python3
"""Execute the canonical API gate with deterministic tool observations."""

import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest
import _strict_unittest as strict_unittest
from public_api_selection import selected_crates
from _native_shell import bash_executable
from _native_test_launcher import python_script_header


SOURCE = Path(__file__).resolve().parent
SURFACE = b"pub mod fixture\n"


class PublicApiTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-api-gate-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        scripts = self.root / "scripts/ci"
        scripts.mkdir(parents=True)
        for name in ("check_public_api.sh", "_cargo_target.sh", "public_api_selection.py"):
            shutil.copy2(SOURCE / name, scripts / name)
        self.script = scripts / "check_public_api.sh"
        self.snapshot = self.root / "public-api/fixture.txt"
        binary = self.root / "bin"
        binary.mkdir()
        tool = binary / "tool"
        tool.write_text(python_script_header(sys.executable) + '''import json, os, sys
from pathlib import Path
sys.stdout.reconfigure(newline="\\n")
name, args = Path(sys.argv[0]).name, sys.argv[1:]
if name == "rustc":
    sys.exit(1 if os.environ.get("API_TEST_NO_NIGHTLY") else 0)
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
    packages = [{"name": "fixture", "metadata": {"awaken": {"api_snapshot": True}}}]
    if os.environ.get("API_TEST_SECOND_SELECTED_MEMBER"):
        packages.append({"name": "zzsecond", "metadata": {"awaken": {"api_snapshot": True}}})
    if "API_TEST_SNAPSHOT_TARGET" in os.environ:
        packages[0]["metadata"]["awaken"]["api_snapshot_target"] = os.environ["API_TEST_SNAPSHOT_TARGET"]
    if os.environ.get("API_TEST_SELECTED_PRODUCT"):
        packages[0]["metadata"]["awaken"].update(
            product=os.environ["API_TEST_SELECTED_PRODUCT"], separation="contract")
        packages[0]["publish"] = json.loads(os.environ.get("API_TEST_SELECTED_PUBLISH", "null"))
    if os.environ.get("API_TEST_UNSELECTED_MEMBER"):
        metadata = {}
        if os.environ.get("API_TEST_UNSELECTED_PRODUCT"):
            metadata.update(product=os.environ["API_TEST_UNSELECTED_PRODUCT"], separation="contract")
        if os.environ.get("API_TEST_UNSELECTED_SNAPSHOT"):
            metadata["api_snapshot"] = json.loads(os.environ["API_TEST_UNSELECTED_SNAPSHOT"])
        packages.append({"name": "internal", "metadata": {"awaken": metadata},
                         "publish": json.loads(os.environ.get("API_TEST_UNSELECTED_PUBLISH", "null"))})
    print(json.dumps({"packages": packages}))
else:
    package = args[3] if len(args) > 3 else ""
    assert package == "fixture" or (package == "zzsecond" and os.environ.get("API_TEST_SECOND_SELECTED_MEMBER")), args
    expected = ["+nightly-2026-07-14", "public-api", "-p", package,
                "--simplified", "--omit", "blanket-impls,auto-trait-impls,auto-derived-impls"]
    if package == "fixture" and os.environ.get("API_TEST_SNAPSHOT_TARGET"):
        expected += ["--target", os.environ["API_TEST_SNAPSHOT_TARGET"]]
    assert args == expected, args
    Path(os.environ["API_TEST_COMPUTED"]).write_text(json.dumps(args))
    if os.environ.get("API_TEST_ARGV_LOG"):
        with Path(os.environ["API_TEST_ARGV_LOG"]).open("a") as log:
            log.write(json.dumps(args) + "\\n")
    if os.environ.get("API_TEST_COMPUTE_ERROR"):
        sys.exit(74)
    print("pub mod " + package)
''', encoding="utf-8", newline="\n")
        tool.chmod(0o755)
        for name in ("cargo", "cargo-public-api", "rustc"):
            alias = binary / name
            shutil.copyfile(tool, alias)
            alias.chmod(0o755)
        real_diff = shutil.which("diff")
        if real_diff is None and os.name == "nt":
            real_diff = next((str(parent / "usr/bin/diff.exe")
                for parent in Path(bash_executable()).parents
                if (parent / "usr/bin/diff.exe").is_file()), None)
        self.assertIsNotNone(real_diff, "the fixture requires the native diff program")
        # Keep process-substitution descriptors inside the shell runtime. A
        # native Python execv adapter loses MSYS descriptors and child status.
        diff = binary / "diff"
        diff.write_text('#!/bin/sh\n'
                        'if [ -n "${API_TEST_DIFF_ERROR:-}" ]; then exit 2; fi\n'
                        'exec "$API_TEST_REAL_DIFF" "$@"\n', encoding="utf-8", newline="\n")
        diff.chmod(0o755)
        # Git Bash initializes its own utility PATH on Windows. Reassert the
        # owned tools after that boundary so R5 cannot accidentally use real diff.
        setup = self.root / "fixture-environment.sh"
        setup.write_text('fixture_bin=$(cd -- "$API_TEST_BIN" && pwd -P)\n'
                         'export PATH="$fixture_bin:$PATH"\n', encoding="utf-8", newline="\n")
        self.environment = dict(
            os.environ, PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
            API_TEST_REAL_DIFF=real_diff,
            API_TEST_COMPUTED=str(self.root / "computed"),
            API_TEST_BIN=binary.as_posix(), BASH_ENV=setup.as_posix(),
        )

    def test_authored_platform_target_is_passed_literally_and_invalid_targets_never_bless(self):
        import json
        self.seed(SURFACE)
        result = self.invoke("--require-tools", API_TEST_SNAPSHOT_TARGET="x86_64-pc-windows-msvc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads((self.root / "computed").read_text())[-2:],
                         ["--target", "x86_64-pc-windows-msvc"])
        for target in ("", "--target", "x86_64;touch injected", "x86_64\nother", "$(false)"):
            for mode in ("--require-tools", "--bless"):
                with self.subTest(target=target, mode=mode):
                    (self.root / "computed").unlink(missing_ok=True)
                    result = self.invoke(mode, API_TEST_SNAPSHOT_TARGET=target)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("literal target triple", result.stderr)
                    self.assertFalse((self.root / "computed").exists())
                    self.assertEqual(self.snapshot.read_bytes(), SURFACE)
        with self.assertRaises(ValueError):
            selected_crates([{"name": "unselected", "metadata": {"awaken": {
                "api_snapshot_target": "x86_64-pc-windows-msvc"}}}])

    def test_platform_target_does_not_leak_to_the_next_selected_crate(self):
        import json
        self.seed(SURFACE)
        second = self.root / "public-api/zzsecond.txt"
        second.write_bytes(b"pub mod zzsecond\n")
        log = self.root / "api-argv.jsonl"
        environment = dict(API_TEST_SNAPSHOT_TARGET="x86_64-pc-windows-msvc",
                           API_TEST_SECOND_SELECTED_MEMBER="1", API_TEST_ARGV_LOG=str(log))
        result = self.invoke("--require-tools", **environment)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([call[3] for call in calls], ["fixture", "zzsecond"])
        self.assertEqual(calls[0][-2:], ["--target", "x86_64-pc-windows-msvc"])
        self.assertNotIn("--target", calls[1])
        # Executed negative control: keep the previous member's target. The
        # exact tool-argv oracle must reject it and preserve both baselines.
        code = self.script.read_text()
        self.assertEqual(code.count("  target_args=()\n"), 1)
        self.script.write_text(code.replace("  target_args=()\n", ""), encoding="utf-8", newline="\n")
        result = self.invoke("--require-tools", **environment)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.snapshot.read_bytes(), SURFACE)
        self.assertEqual(second.read_bytes(), b"pub mod zzsecond\n")

    def test_python_interpreter_path_preserves_spaces_quotes_and_arguments(self):
        # H1 real interpreter in a spaced/quoted path; H2 empty, quoted, trailing
        # backslash and UTF-8 argv; H3 child0/31 retain both bytes and status.
        import json
        interpreter = self.root / "Python interpreter's folder" / "python proxy.exe"
        interpreter.parent.mkdir()
        environment = dict(os.environ)
        if os.name == "nt":
            from _native_test_launcher import native_fixture_launcher
            shutil.copyfile(native_fixture_launcher(), interpreter)
            forward = self.root / "forward.py"
            forward.write_text("import subprocess,sys\nraise SystemExit(subprocess.call([sys.executable,*sys.argv[1:]]))\n", encoding="utf-8", newline="\n")
            environment.update(AWAKEN_FIXTURE_PROGRAM=sys.executable, AWAKEN_FIXTURE_SCRIPT=str(forward))
        else:
            interpreter.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + ' "$@"\n', encoding="utf-8", newline="\n")
            interpreter.chmod(0o755)
        script = self.root / "header-probe"
        script.write_text(python_script_header(interpreter) + 'import json,sys\nsys.stdout.buffer.write(json.dumps(sys.argv[1:-1],ensure_ascii=False).encode("utf-8"))\nraise SystemExit(int(sys.argv[-1]))\n', encoding="utf-8", newline="\n")
        script.chmod(0o755)
        arguments = ["", "quoted ' \" argument", "C:\\path with spaces\\", "literal /bin/sh;$(value) 中文"]
        for status in (0, 31):
            with self.subTest(status=status):
                result = subprocess.run([bash_executable(), "-c", 'exec "$@"', "header-probe", script.as_posix(), *arguments, str(status)], env=environment, capture_output=True, timeout=30)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(json.loads(result.stdout), arguments)

    def invoke(self, *arguments, **conditions):
        return subprocess.run(
            [bash_executable(), str(self.script), *arguments], cwd=self.root,
            env=dict(self.environment, **conditions), capture_output=True,
            text=True, timeout=10,
        )

    def seed(self, content):
        self.snapshot.parent.mkdir(exist_ok=True)
        self.snapshot.write_bytes(content)

    def test_selection_is_positive_and_an_unselected_snapshot_is_an_orphan(self):
        # S1 a member without api_snapshot is never computed (the fake cargo
        # refuses any `-p` but the fixture) and the gate still passes on the
        # selected one; S2 a snapshot file for an unselected member is an
        # orphan and fails before anything is computed, in check and in bless.
        # SDK completeness cause/effect table: C1 product SDK vs internal;
        # C2 Cargo publish null/registry list vs [] (publish=false); C3 explicit
        # api_snapshot true vs omitted/false. S3-S6 C1+publishable+!C3 => refuse
        # before generation or baseline write, even under bless. S7 SDK+[] and
        # S8 internal+publishable do not require selection, preserving S1/S2.
        # S9 explicit selection is still computed for SDK/internal x null/list/
        # [], including an unpublished member with its own selection reason.
        # Separation is not the publication authority. The original
        # mocked IO edge rejects any unexpected crate or rendering argv.
        # S10 the exact same metadata owner yields two projections: explicit
        # snapshots include internal/unpublished members; SDK qualification
        # includes only publishable SDK members. Both reject missing selection.
        for product in ("sdk", "platform"):
            for publish in (None, ["private-registry"], []):
                row = {"name": "fixture", "publish": publish,
                       "metadata": {"awaken": {"product": product, "api_snapshot": True}}}
                snapshots, sdk = selected_crates([row])
                self.assertEqual(snapshots, ["fixture"])
                self.assertEqual(sdk, ["fixture"] if product == "sdk" and publish != [] else [])
        self.seed(SURFACE)
        result = self.invoke("--require-tools", API_TEST_UNSELECTED_MEMBER="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        orphan = self.root / "public-api/internal.txt"
        orphan.write_bytes(b"pub mod internal\n")
        for arguments in (("--require-tools",), ("--bless", "--require-tools")):
            with self.subTest(arguments=arguments):
                result = self.invoke(*arguments, API_TEST_UNSELECTED_MEMBER="1")
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("without a selecting crate", result.stderr)
                self.assertTrue(orphan.exists(), "an orphan is reported, never deleted")
        orphan.unlink()
        for rule, product, publish, selection, expected in (
            ("S3", "sdk", "null", "", 1),
            ("S4", "sdk", '["private-registry"]', "", 1),
            ("S5", "sdk", "null", "false", 1),
            ("S6", "sdk", '["private-registry"]', "false", 1),
            ("S7 omitted", "sdk", "[]", "", 0),
            ("S7 false", "sdk", "[]", "false", 0),
            ("S8 omitted", "platform", "null", "", 0),
            ("S8 false", "platform", "null", "false", 0),
        ):
            for arguments in (("--require-tools",), ("--bless", "--require-tools")):
                with self.subTest(rule=rule, arguments=arguments):
                    (self.root / "computed").unlink(missing_ok=True)
                    result = self.invoke(
                        *arguments, API_TEST_UNSELECTED_MEMBER="1",
                        API_TEST_UNSELECTED_PRODUCT=product,
                        API_TEST_UNSELECTED_PUBLISH=publish,
                        API_TEST_UNSELECTED_SNAPSHOT=selection,
                    )
                    self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                    self.assertEqual((self.root / "computed").exists(), expected == 0)
                    self.assertEqual(self.snapshot.read_bytes(), SURFACE, "no missing selection may bless")
                    if expected:
                        self.assertIn("publishable SDK crates must explicitly select", result.stderr)
                        self.assertIn("internal", result.stderr)
        for product in ("sdk", "platform"):
            for publish in ("null", '["private-registry"]', "[]"):
                with self.subTest(rule="S9", product=product, publish=publish):
                    result = self.invoke(
                        "--require-tools", API_TEST_SELECTED_PRODUCT=product,
                        API_TEST_SELECTED_PUBLISH=publish,
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("1 crates compared", result.stdout)

    def test_check_is_read_only_and_requires_a_comparable_reviewed_baseline(self):
        # Causes: C1 baseline exists; C2 computed surface matches; C3 generation
        # succeeds; C4 diff returns a valid equal/different observation. Effects:
        # E1 only C1+C2+C3+C4 passes; E2 every other row fails; E3 check never
        # writes/creates the baseline, including a missing parent directory.
        # R1 equal; R2 drift; R3 missing file; R4 missing directory; R5 diff
        # observer error; R6 generator error. C2 is meaningful only with C1+C3.
        # Run these same baseline/compute/diff partitions through an explicitly
        # selected publishable SDK; completeness cannot waive baseline review.
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
                result = self.invoke("--require-tools", API_TEST_SELECTED_PRODUCT="sdk", **conditions)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(self.snapshot.parent.exists(), parent, "E3 directory mutation")
                actual = self.snapshot.read_bytes() if self.snapshot.exists() else None
                self.assertEqual(actual, baseline, "E3 snapshot mutation")

    def test_only_explicit_bless_writes_successfully_computed_surface(self):
        # B1 missing baseline + explicit bless + successful generation => create;
        # B2 changed baseline => replace; B3 failed generation => retain exact
        # original bytes and fail, never publish empty/partial tool output.
        # The selected SDK partition keeps the same sole explicit writer.
        for rule, baseline, conditions, expected in (
            ("B1", None, {}, 0),
            ("B2", b"reviewed old bytes", {}, 0),
            ("B3", b"reviewed old bytes", {"API_TEST_COMPUTE_ERROR": "1"}, 1),
        ):
            with self.subTest(rule=rule):
                self.snapshot.unlink(missing_ok=True)
                if baseline is not None:
                    self.seed(baseline)
                result = self.invoke("--bless", "--require-tools", API_TEST_SELECTED_PRODUCT="sdk", **conditions)
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
        --bless. An unselected publishable SDK in the metadata fixture cannot
        make a tooling refusal succeed; an explicit absent-tool waiver still
        says that nothing was compared, rather than certifying SDK coverage.
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
                    result = self.invoke(
                        "--bless", *flags, API_TEST_UNSELECTED_MEMBER="1",
                        API_TEST_UNSELECTED_PRODUCT="sdk", **conditions,
                    )
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

        # No snapshot is seeded for Z1: with an empty inventory a seeded file
        # would be an orphan, which is refused earlier and for its own reason.
        empty = self.invoke("--require-tools", API_TEST_EMPTY_WORKSPACE="1")
        self.assertNotEqual(empty.returncode, 0, empty.stdout + empty.stderr)
        self.assertIn("compared 0 crates", empty.stderr)
        self.assertFalse((self.root / "computed").exists())
        self.seed(SURFACE)
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
    strict_unittest.main()
