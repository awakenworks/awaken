"""Bootstrap publication rules; compiler semantics are checked by the real Kani fixture."""
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile
import unittest
import _strict_unittest as strict_unittest
from unittest import mock
from _native_shell import bash_executable
from formal_sources import detached_git_environment
from test_formal_run_evidence import nested_shell_function, run_shell_fixture

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
    backend = source is not None and source.name == "source"
    prefix = "CBMC_" if backend else ""
    action = args[0]
    if action in ("clone", "init"):
        if action == "clone":
            source = pathlib.Path(args[-1])
        (source / ".git").mkdir(parents=True)
        (source / "scripts").mkdir()
        (source / "scripts/kani").symlink_to(os.environ["KANI_WITNESS"])
    elif action == "rev-parse":
        print(os.environ.get(prefix + "ACTUAL_REVISION", os.environ[prefix + "PINNED_REVISION"]))
    elif action == "ls-files":
        print("unexpected.rs" if os.environ.get(prefix + "UNTRACKED") else "")
    elif action == "apply" and "--numstat" in args:
        pass  # Real Git path/index behavior belongs to ActualPatchSourceTests.
    elif action == "apply" and "--check" not in args:
        (source / "applied").touch()
    elif action == "diff":
        if "--cached" in args:
            sys.exit(int(os.environ.get(prefix + "STAGED", "0")))
        if "--quiet" in args:
            sys.exit(int((source / "applied").exists() or bool(os.environ.get(prefix + "DRIFT"))))
        if (source / "applied").exists():
            patch = pathlib.Path(os.environ[prefix + "PATCH"]).read_text()
            if os.environ.get(prefix + "CONTEXT_SPACES"):
                patch = "\n".join(" " if line == "" else line for line in patch.splitlines()) + "\n"
            sys.stdout.write(patch)
        if os.environ.get(prefix + "DRIFT"):
            print("unreviewed change")
elif name == "cmake":
    if args[0] == "--build":
        build = pathlib.Path(args[1])
        (build / "bin").mkdir(parents=True, exist_ok=True)
        for binary in ("cbmc", "unit"):
            target = build / "bin" / binary
            target.write_bytes(pathlib.Path(sys.argv[0]).read_bytes()); target.chmod(0o755)
        sys.exit(int(os.environ.get("CBMC_BUILD_EXIT", "0")))
    build = pathlib.Path(args[args.index("-B") + 1]); build.mkdir(parents=True)
    (build / "CMakeCache.txt").write_text(" ".join(args))
elif name == "unit":
    print("All tests passed")
    sys.exit(int(os.environ.get("Z3_EXIT" if args == ["[smt2-z3]"] else "UNIT_EXIT", "0")))
elif name == "goto-cc":
    pathlib.Path(args[args.index("-o") + 1]).write_text("goto")
elif name == "cbmc":
    negative = args[-1].endswith(".goto")
    print("[main.assertion.1] force typed pointer discovery: FAILURE" if negative else "VERIFICATION SUCCESSFUL")
    sys.exit(int(os.environ.get("NEGATIVE_EXIT" if negative else "POSITIVE_EXIT", "10" if negative else "0")))
elif name == "cargo":
    if args[:1] == ["test"]:
        if (args != ["test", "--locked", "-p", "kani-driver", "artifact_profile", "--", "--nocapture"]
                or os.environ.get("RUSTUP_TOOLCHAIN") != os.environ["PINNED_TOOLCHAIN"]):
            print("driver artifact-profile qualification differs", file=sys.stderr)
            sys.exit(32)
        print(os.environ.get("DRIVER_TEST_OUTPUT", "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"))
        sys.exit(int(os.environ.get("DRIVER_TEST_EXIT", "0")))
    if args[:1] in (["metadata"], ["kani"]):
        expected = os.environ.get("EXPECT_POST_BUILD_TOOLCHAIN")
        if expected is not None and os.environ.get("RUSTUP_TOOLCHAIN") != expected:
            print("Kani build changed the caller's proof toolchain", file=sys.stderr)
            sys.exit(31)
    if args[:1] == ["metadata"]:
        if "--locked" not in args or "--no-deps" not in args:
            print("constructor lock admission differs", file=sys.stderr)
            sys.exit(29)
        sys.exit(int(os.environ.get("CONSTRUCTOR_METADATA_EXIT", "0")))
    if args[:1] == ["kani"]:
        if "--verbose" not in args:
            print("verbose constructor statistics fixture was not selected", file=sys.stderr)
            sys.exit(25)
        if ("--exact" not in args or "--locked" in args
                or args[args.index("--harness") + 1] != "imported_constructor_preserves_its_value"
                or not args[args.index("--manifest-path") + 1].endswith("/fixtures/kani_constructor/Cargo.toml")):
            print("constructor fixture selection differs", file=sys.stderr)
            sys.exit(27)
        target = args[args.index("--target-dir") + 1]
        if (os.environ.get("CARGO_TARGET_DIR") != target
                or os.environ.get("CARGO_BUILD_BUILD_DIR") != target + "/kani"):
            print("constructor driver target differs", file=sys.stderr)
            sys.exit(28)
        transcript = os.environ.get("CONSTRUCTOR_OUTPUT", "exact")
        if transcript != "empty":
            case = "renamed_constructor" if transcript == "renamed" else "imported_constructor_preserves_its_value"
            print("Checking harness " + case + "...")
            if transcript == "zero":
                print("Complete - 0 successfully verified harnesses, 0 failures, 0 total.")
            elif transcript == "failed":
                print("Complete - 0 successfully verified harnesses, 1 failures, 1 total.")
            else:
                print("Complete - 1 successfully verified harnesses, 0 failures, 1 total.")
        if os.environ.get("CONSTRUCTOR_LOCK_DRIFT"):
            lock = pathlib.Path(args[args.index("--manifest-path") + 1]).with_name("Cargo.lock")
            lock.write_bytes(lock.read_bytes() + b"\n# drift\n")
        sys.exit(int(os.environ.get("CONSTRUCTOR_EXIT", "0")))
    if args != ["build-dev"] or os.environ.get("RUSTUP_TOOLCHAIN") != os.environ["PINNED_TOOLCHAIN"]:
        print("Kani build did not select its pinned toolchain", file=sys.stderr)
        sys.exit(30)
    build = pathlib.Path.cwd() / "target/kani"
    build.mkdir(parents=True, exist_ok=True)
    (build / "compiler").write_text("compiled bytes")
    sys.exit(int(os.environ.get("BUILD_EXIT", "0")))
elif name == "kani":
    sys.exit(int(os.environ.get("REGRESSION_EXIT", "0")))
'''


@unittest.skipIf(os.name == "nt", "POSIX Kani publication and symlink laws run in the Linux gate")
class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-kani-bootstrap-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.scripts = self.root / "scripts/ci"
        for relative in ("bootstrap_kani.sh", "_provider_environment.sh", "_cargo_target.sh",
                         "patches/kani-wide-niche.patch",
                         "fixtures/kani_wide_niche.rs", "patches/cbmc-smt-type-dependencies.patch",
                         "fixtures/cbmc_recursive_alias.c",
                         "fixtures/kani_constructor/Cargo.toml", "fixtures/kani_constructor/Cargo.lock",
                         "fixtures/kani_constructor/model/Cargo.toml",
                         "fixtures/kani_constructor/model/src/lib.rs", "fixtures/kani_constructor/src/lib.rs"):
            target = self.scripts / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "scripts/ci" / relative, target)
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
        for name in ("git", "cargo", "rustup", "kani", "cbmc", "goto-cc", "goto-instrument", "cmake", "z3"):
            tool = bindir / name
            tool.write_text(TOOL)
            tool.chmod(0o755)
        # The only replaced port is the release installation directory; all
        # source/build/qualification and selection rules execute the real shell.
        source = source.replace('$HOME/.kani/kani-$kani_backend_version/bin', str(bindir))
        (self.scripts / "bootstrap_kani.sh").write_text(source)
        # Tool/disk admission is an independent port owned by gate_tools tests;
        # retain both exact calls in the real selection slice, refusing others.
        (self.scripts / "gate_tools.py").write_text(
            "import sys\nassert sys.argv[1:] in "
            "[['require', '--gate', 'formal'], ['disk', '--stage', 'formal']]\n"
        )
        self.calls = self.root / "calls"
        self.env = dict(os.environ, PATH=f"{bindir}:{os.environ['PATH']}",
                        AWAKEN_KANI_CACHE_DIR=str(self.cache),
                        CALLS=str(self.calls),
                        CBMC_PINNED_REVISION=re.search(r'readonly cbmc_revision="([^"]+)"', source)[1],
                        CBMC_PATCH=str(self.scripts / "patches/cbmc-smt-type-dependencies.patch"),
                        PINNED_REVISION=revision,
                        PINNED_TOOLCHAIN=re.search(r'readonly kani_nightly="([^"]+)"', source)[1],
                        PATCH=str(self.scripts / "patches/kani-wide-niche.patch"),
                        KANI_WITNESS=str(bindir / "kani"))
        # The fixture owns tool selection; callers may explicitly override it
        # through run_bootstrap/verify, but ambient production selections cannot.
        self.env.pop("AWAKEN_KANI_SOURCE_DIR", None)
        self.env.pop("AWAKEN_KANI_BACKEND_DIR", None)

    def run_bootstrap(self, **changes):
        return subprocess.run(["bash", str(self.scripts / "bootstrap_kani.sh")],
                              env=dict(self.env, **changes), capture_output=True, text=True)

    def test_foreign_parent_toolchain_cannot_override_the_pinned_compiler_build(self):
        # A1: the admitted main-workspace parent carries 1.96.0; A2: Kani's
        # source requires its own nightly. E1: build-dev and nested Cargo see
        # exactly the reviewed pin; E2: later fixture commands keep the caller's
        # choice; E3: only successful semantic qualification publishes current.
        # The instrumented Cargo rejects the wrong build/proof selection with
        # distinct statuses 30/31, while the real bootstrap owns the boundary.
        result = self.run_bootstrap(RUSTUP_TOOLCHAIN="1.96.0",
                                    EXPECT_POST_BUILD_TOOLCHAIN="1.96.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo build-dev\n", self.calls.read_text())
        self.assertEqual((self.cache / "current").resolve(), self.patched)
        self.assertTrue((self.patched / "target/awaken-kani-build.sha256").is_file())

    def test_ambient_selectors_cannot_escape_the_owned_fixture(self):
        # I1 neither ambient selector, I2 source only, I3 backend only, I4 both:
        # every case must build/select the fixture's own qualified package and
        # leave the external sentinel unchanged. This covers check_formal.sh
        # exporting its frozen real backend before running these same tests.
        # Explicit per-call selection remains covered by B5 and R8 below.
        selectors = ("AWAKEN_KANI_SOURCE_DIR", "AWAKEN_KANI_BACKEND_DIR")
        external = self.root / "ambient-package"
        external.mkdir()
        sentinel = external / "keep"
        sentinel.write_bytes(b"caller-owned toolchain")
        for mask in range(4):
            with self.subTest(rule=f"I{mask + 1}"):
                ambient = {key: value for key, value in os.environ.items()
                           if key not in selectors}
                ambient.update({key: str(external / key) for bit, key in enumerate(selectors)
                                if mask & (1 << bit)})
                fixture = BootstrapTests(
                    "test_success_and_exact_cache_replay_publish_only_after_qualification")
                try:
                    with mock.patch.dict(os.environ, ambient, clear=True):
                        fixture.setUp()
                    result = fixture.run_bootstrap()
                    self.assertEqual(result.returncode, 0, result.stderr)
                    selected = subprocess.run(
                        ["bash", str(fixture.scripts / "bootstrap_kani.sh"), "--paths"],
                        env=fixture.env, capture_output=True, text=True)
                    self.assertEqual(selected.returncode, 0, selected.stderr)
                    self.assertEqual(selected.stdout.splitlines(),
                                     [str(fixture.patched), str(fixture.backend() / "bin")])
                    self.assertNotIn(str(external), fixture.calls.read_text())
                    self.assertEqual(list(external.iterdir()), [sentinel])
                    self.assertEqual(sentinel.read_bytes(), b"caller-owned toolchain")
                finally:
                    fixture.doCleanups()

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
        applied = [line for line in calls if " apply " in line
                   and "--check" not in line and "--numstat" not in line]
        self.assertEqual(len(applied), 2)
        builds = [i for i, line in enumerate(calls) if line == "cargo build-dev"]
        checks = [i for i, line in enumerate(calls) if line.startswith("kani ")]
        self.assertEqual(len(checks), 2)
        self.assertEqual(len(builds), 1, "qualified cache must never rebuild in place")
        self.assertTrue(all(builds[0] < check for check in checks))
        self.assertTrue(all("--target-dir" in calls[i] and "kani_wide_niche.rs" in calls[i] for i in checks))

    def test_driver_profile_binding_and_renderer_are_required_before_publication(self):
        # Causes D1 the real driver's artifact_profile tests pass; D2 they fail;
        # D3 bootstrap omits/mis-selects that required test invocation. Effects:
        # D1 permits later proof qualification; D2/D3 preserve old selection and
        # publish no build receipt. The port models only process outcome; Rust
        # unit tests in the reviewed patch own actual map and JSON behavior.
        result = self.run_bootstrap(DRIVER_TEST_EXIT="23")
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.previous)
        self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())
        self.assertNotIn("kani ", self.calls.read_text())
        self.assertIn("cargo test --locked -p kani-driver artifact_profile -- --nocapture", self.calls.read_text())

    def test_driver_tests_cannot_qualify_an_empty_or_skipped_selection(self):
        # D4 exit zero with absent/zero/ignored test execution cannot publish a
        # build. All four actual binder/renderer cases must execute and pass.
        for report in ("", "test result: ok. 0 passed; 0 failed; 0 ignored;",
                       "test result: ok. 3 passed; 0 failed; 1 ignored;"):
            with self.subTest(report=report):
                self.assertNotEqual(self.run_bootstrap(DRIVER_TEST_OUTPUT=report).returncode, 0)
                self.assertEqual((self.cache / "current").resolve(), self.previous)
                self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())

    def test_verbose_constructor_statistics_qualify_before_compiler_publication(self):
        # V1 complete constructor qualification => publish. V2 dropping verbose
        # refuses before publication. V3 its own failure preserves the previous
        # selection; successful wide-niche qualification alone is insufficient.
        script = self.scripts / "bootstrap_kani.sh"
        source = script.read_text()
        marker = '--output-format terse --verbose'
        self.assertIn(marker, source)
        script.write_text(source.replace(marker, '--output-format terse'))
        result = self.run_bootstrap()
        self.assertEqual(result.returncode, 25, result.stderr)
        self.assertIn("verbose constructor statistics fixture was not selected", result.stdout + result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.previous)
        self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())
        self.assertFalse(any(self.cache.glob("cbmc-*/qualified-*.txt")))

    def test_constructor_requires_exact_executed_success_before_publication(self):
        # C1 exact single success publishes (covered above). C2 no evidence,
        # C3 renamed selection, C4 zero harnesses, C5 failed harness with a
        # falsely successful tool status all refuse and preserve current.
        for output in ("empty", "renamed", "zero", "failed"):
            with self.subTest(output=output):
                result = self.run_bootstrap(CONSTRUCTOR_OUTPUT=output)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertEqual((self.cache / "current").resolve(), self.previous)
                self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())
                self.assertFalse(any(self.cache.glob("cbmc-*/qualified-*.txt")))

    def test_constructor_owns_its_driver_layout_under_ambient_target_selectors(self):
        # T1 absent selectors and T2 caller-owned target/build-dir must both
        # execute in the constructor's own driver layout (the tool port checks
        # exact environment/argv equality). Caller-owned bytes remain intact.
        external = self.root / "caller-target"
        external.mkdir()
        sentinel = external / "keep"
        sentinel.write_bytes(b"caller-owned build cache")
        result = self.run_bootstrap(CARGO_TARGET_DIR=str(external),
                                    CARGO_BUILD_BUILD_DIR=str(external / "elsewhere"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.patched)
        self.assertEqual(list(external.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_bytes(), b"caller-owned build cache")

    def test_constructor_refuses_missing_driver_layout_authority(self):
        script = self.scripts / "bootstrap_kani.sh"
        source = script.read_text()
        call = "  awaken_configure_kani_driver_target\n"
        self.assertIn(call, source)
        script.write_text(source.replace(call, ""))
        result = self.run_bootstrap(CARGO_BUILD_BUILD_DIR=str(self.root / "wrong"))
        self.assertEqual(result.returncode, 28, result.stdout + result.stderr)
        self.assertIn("constructor driver target differs", result.stdout + result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.previous)
        self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())

    def test_constructor_lock_admission_is_native_and_required(self):
        script = self.scripts / "bootstrap_kani.sh"
        source = script.read_text()
        call = "cargo metadata --locked --no-deps"
        self.assertIn(call, source)
        script.write_text(source.replace(call, "cargo metadata --no-deps"))
        result = self.run_bootstrap()
        self.assertEqual(result.returncode, 29, result.stdout + result.stderr)
        self.assertIn("constructor lock admission differs", result.stdout + result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.previous)
        self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())

    def test_constructor_lock_drift_refuses_even_after_successful_proof(self):
        result = self.run_bootstrap(CONSTRUCTOR_LOCK_DRIFT="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("constructor fixture lock changed during qualification", result.stderr)
        self.assertEqual((self.cache / "current").resolve(), self.previous)
        self.assertFalse((self.patched / "target/awaken-kani-build.sha256").exists())
        self.assertFalse(any(self.cache.glob("cbmc-*/qualified-*.txt")))

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
            ({"CONSTRUCTOR_EXIT": "26"}, True, True),
            ({"CONSTRUCTOR_METADATA_EXIT": "29"}, True, True),
        ):
            with self.subTest(changes=changes):
                self.calls.write_text("")
                result = self.run_bootstrap(**changes)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((self.cache / "current").resolve(), self.previous)
                self.assertFalse(any(self.cache.glob("cbmc-*/qualified-*.txt")))
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

    def test_empty_context_spelling_does_not_hide_source_changes(self):
        # W1 Git's one-space empty context and W2 the reviewed empty spelling
        # describe identical source -> same exact-patch admission. W3 a real
        # extra source diff remains rejected. Only patch syntax is normalized;
        # added/deleted source whitespace must never become an ignored axis.
        self.assertEqual(self.run_bootstrap(CONTEXT_SPACES="1").returncode, 0, "W1")
        self.assertEqual(self.run_bootstrap().returncode, 0, "W2")
        self.assertNotEqual(self.verify(CONTEXT_SPACES="1", DRIFT="1").returncode, 0, "W3")

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

    def backend(self):
        return next(self.cache.glob("cbmc-*"))

    def procedure_key(self):
        """The fixture script's own qualification digest, derived as the script derives it."""
        files = [self.scripts / "bootstrap_kani.sh",
                 self.scripts / "fixtures/cbmc_recursive_alias.c",
                 self.scripts / "fixtures/kani_wide_niche.rs",
                 self.scripts / "_cargo_target.sh",
                 self.scripts / "fixtures/kani_constructor/Cargo.toml",
                 self.scripts / "fixtures/kani_constructor/Cargo.lock",
                 self.scripts / "fixtures/kani_constructor/model/Cargo.toml",
                 self.scripts / "fixtures/kani_constructor/model/src/lib.rs",
                 self.scripts / "fixtures/kani_constructor/src/lib.rs"]
        lines = "".join(hashlib.sha256(path.read_bytes()).hexdigest() + "\n" for path in files)
        return hashlib.sha256(lines.encode()).hexdigest()[:12]

    def receipt(self, package):
        return package / f"qualified-{self.procedure_key()}.txt"

    def verify(self, **changes):
        return subprocess.run(["bash", str(self.scripts / "bootstrap_kani.sh"),
                               "--verify-source", str(self.patched)],
                              env=dict(self.env, **changes), capture_output=True, text=True)

    def test_backend_decision_table_never_publishes_unqualified_tools(self):
        # B1 revision/staged/untracked/patch drift -> no compilation. B2 compiler,
        # unit, real-Z3 or positive regression failure -> no receipt/publication.
        # B3 intentional negative must fail as a property (10), never succeed
        # (0) or throw an encoder exception (6). Each row starts a fresh cache.
        for changes, builds in (
            ({"CBMC_ACTUAL_REVISION": "wrong"}, False),
            ({"CBMC_STAGED": "1"}, False), ({"CBMC_UNTRACKED": "1"}, False),
            ({"CBMC_DRIFT": "1"}, False), ({"CBMC_BUILD_EXIT": "23"}, True),
            ({"UNIT_EXIT": "24"}, True), ({"Z3_EXIT": "25"}, True),
            ({"POSITIVE_EXIT": "6"}, True), ({"NEGATIVE_EXIT": "0"}, True),
            ({"NEGATIVE_EXIT": "6"}, True),
        ):
            with self.subTest(changes=changes):
                for package in self.cache.glob("cbmc-*"):
                    shutil.rmtree(package)
                self.calls.write_text("")
                result = self.run_bootstrap(**changes)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual((self.cache / "current").resolve(), self.previous)
                self.assertFalse(any(self.cache.glob("cbmc-*/qualified-*.txt")))
                self.assertEqual("cmake --build" in self.calls.read_text(), builds)
                self.assertNotIn("kani ", self.calls.read_text())

    def test_unqualified_destination_links_cannot_overwrite_previous_tools(self):
        # B6 a failed attempt leaves destinations linked to an older package.
        # Retrying may replace its own links, but must preserve every old byte.
        self.assertNotEqual(self.run_bootstrap(UNIT_EXIT="23").returncode, 0)
        backend = self.backend()
        old = self.previous / "old-tool"
        old.write_bytes(b"qualified old bytes")
        for name in ("cbmc", "goto-cc", "goto-instrument"):
            destination = backend / "bin" / name
            destination.unlink()
            if name == "goto-cc":
                os.link(old, destination)
            else:
                destination.symlink_to(old)
        pending = f"qualified-{self.procedure_key()}.pending"
        for output in (pending, "unit.log", "z3.log", "leaf.goto", "internal.log"):
            destination = backend / output
            destination.unlink(missing_ok=True)
            destination.symlink_to(old)
        result = self.run_bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(old.read_bytes(), b"qualified old bytes")
        self.assertTrue(all(not (backend / "bin" / name).is_symlink()
                            for name in ("cbmc", "goto-cc", "goto-instrument")))

    def test_unqualified_source_alias_never_patches_a_previous_source(self):
        # B7 even a clean, correctly pinned source is not writable through an
        # alias into a previous cache. Refusal must precede git apply/build.
        self.assertNotEqual(self.run_bootstrap(CBMC_BUILD_EXIT="23").returncode, 0)
        backend = self.backend()
        source = backend / "source"
        previous_source = self.previous / "source"
        source.rename(previous_source)
        (previous_source / "applied").unlink()
        source.symlink_to(previous_source)
        self.calls.write_text("")
        result = self.run_bootstrap()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((previous_source / "applied").exists())
        self.assertNotIn(" apply ", self.calls.read_text())
        self.assertNotIn("cmake ", self.calls.read_text())

    def test_backend_readonly_identity_covers_all_actual_inputs(self):
        # B4 a qualified package is immutable: source, options/configuration,
        # each actual binary path/bytes, fixture/contract and Z3 identity must
        # match. Missing configuration cannot match a truncated receipt. Verify
        # only observes; it cannot rebuild or rerun a green mock to bless drift.
        self.assertEqual(self.run_bootstrap().returncode, 0)
        backend = self.backend()
        good_receipt = self.receipt(backend).read_bytes()
        for relative in ("bin/cbmc", "bin/goto-cc", "bin/goto-instrument", "build/CMakeCache.txt"):
            for action in ("bytes", "missing", "symlink"):
                with self.subTest(relative=relative, action=action):
                    path = backend / relative
                    original = path.read_bytes()
                    mode = path.stat().st_mode
                    if action == "bytes":
                        path.write_bytes(original + b"changed")
                    elif action == "missing":
                        path.unlink()
                    else:
                        target = self.root / "other-target"
                        target.write_bytes(original); target.chmod(mode)
                        path.unlink(); path.symlink_to(target)
                    self.calls.write_text("")
                    result = self.verify()
                    # CMakeCache is input bytes rather than a runtime executable;
                    # an equal-byte path alias does not alter its build options.
                    expected = relative == "build/CMakeCache.txt" and action == "symlink"
                    self.assertEqual(result.returncode == 0, expected, result.stderr)
                    self.assertNotIn("cmake ", self.calls.read_text())
                    self.assertNotIn("unit ", self.calls.read_text())
                    if path.exists() or path.is_symlink():
                        path.unlink()
                    path.write_bytes(original); path.chmod(mode)
                    self.assertEqual(self.receipt(backend).read_bytes(), good_receipt)
        for changes in ({"CBMC_ACTUAL_REVISION": "wrong"}, {"CBMC_DRIFT": "1"},
                        {"CBMC_STAGED": "1"}, {"CBMC_UNTRACKED": "1"}):
            self.assertNotEqual(self.verify(**changes).returncode, 0)
        config = backend / "build/CMakeCache.txt"
        config.unlink()
        self.receipt(backend).write_text(good_receipt.decode().split("configuration_sha256=")[0])
        self.assertNotEqual(self.verify().returncode, 0)

    def test_selected_backend_contract_and_path_shadow_are_rejected(self):
        # B5 changing only the qualification contract/fixture or Z3 bytes must
        # invalidate an explicitly selected old package. A tool earlier in the
        # real driver PATH must not be mistaken for the recorded backend.
        self.assertEqual(self.run_bootstrap().returncode, 0)
        selected = {"AWAKEN_KANI_BACKEND_DIR": str(self.backend() / "bin")}
        for path in (self.scripts / "bootstrap_kani.sh", self.scripts / "fixtures/cbmc_recursive_alias.c",
                     self.scripts / "_cargo_target.sh",
                     self.scripts / "fixtures/kani_constructor/Cargo.toml",
                     self.scripts / "fixtures/kani_constructor/Cargo.lock",
                     self.scripts / "fixtures/kani_constructor/model/Cargo.toml",
                     self.scripts / "fixtures/kani_constructor/model/src/lib.rs",
                     self.scripts / "fixtures/kani_constructor/src/lib.rs",
                     self.root / "bin/z3"):
            original = path.read_bytes()
            path.write_bytes(original + b"\n# drift\n")
            self.assertNotEqual(self.verify(**selected).returncode, 0)
            path.write_bytes(original)
        shadow = self.patched / "scripts/cbmc"
        shadow.write_text("#!/bin/sh\nexit 0\n"); shadow.chmod(0o755)
        result = self.verify(**selected)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PATH selects", result.stderr)
        shadow.unlink()
        self.calls.write_text("")
        result = self.verify(**selected)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("qualification_z3_sha256=", result.stdout)
        self.assertIn("goto-instrument_path=", result.stdout)
        self.assertNotIn("cmake ", self.calls.read_text())

    def test_formal_invocation_freezes_the_selected_compiler_before_current_changes(self):
        # R8 current initially points at A, then another bootstrap publishes B.
        # All later calls in this invocation must still execute and attest A.
        for name in ("A", "B"):
            scripts = self.cache / name / "scripts"
            scripts.mkdir(parents=True)
            tool = scripts / "cargo-kani"
            tool.write_text(f"#!/bin/sh\nprintf '%s\\n' {name}\n")
            tool.chmod(0o755)
        backend_link = self.cache / "selected-backend"
        for name in ("backend-A", "backend-B"):
            (self.cache / name / "bin").mkdir(parents=True)
        backend_link.symlink_to(self.cache / "backend-A/bin")
        current = self.cache / "selected"
        current.symlink_to(self.cache / "A")
        gate = (ROOT / "scripts/ci/check_formal.sh").read_text()
        selection = 'kani_selection=' + gate.split('kani_selection=', 1)[1].split("\nrust_trace_dir=", 1)[0]
        selection = selection.replace('scripts/ci/bootstrap_kani.sh --paths', 'bash scripts/ci/bootstrap_kani.sh --paths')
        script = selection + (
            '\nln -sfn "$CACHE_B" "$AWAKEN_KANI_SOURCE_DIR"'
            '\nln -sfn "$BACKEND_B" "$BACKEND_LINK"'
            '\nprintf "%s\\n" "$kani_source_dir" "$kani_backend_dir"\ncargo-kani\n'
        )
        result = subprocess.run(["bash", "-c", script], cwd=self.root, capture_output=True, text=True,
                                env=dict(self.env, AWAKEN_KANI_SOURCE_DIR=str(current),
                                         CACHE_B=str(self.cache / "B"),
                                         AWAKEN_KANI_BACKEND_DIR=str(backend_link),
                                         BACKEND_LINK=str(backend_link),
                                         BACKEND_B=str(self.cache / "backend-B/bin")))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [str(self.cache / "A"), str(self.cache / "backend-A/bin"), "A"])


    def test_cache_names_the_artifact_and_the_receipt_names_the_judgement(self):
        """The build and its qualification are separate facts.

        | Rule | what changed                          | Effect                            |
        |------|---------------------------------------|-----------------------------------|
        | Q1   | the qualification procedure only      | same package; cmake is never run  |
        | Q2   | the cbmc revision                     | a different package; cmake runs   |
        | Q3   | nothing                               | the package is reused             |
        | Q4   | one published backend binary's bytes  | rebuilt, never reused or blessed  |
        | Q5   | (always)                              | the receipt extends built.txt     |
        | Q6   | the procedure, then back again        | both receipts coexist untouched   |

        Folding the procedure into the package name made Q1 behave like Q2, so
        editing a comment in the bootstrap discarded a built CBMC and demanded a
        rebuild that needs cmake and ninja. Q1 is therefore asserted on the
        instrumented `cmake`: not merely that the run succeeds, but that the
        expensive tool is not reached at all.
        """

        self.assertEqual(self.run_bootstrap().returncode, 0)
        package = self.backend()
        built = (package / "built.txt").read_text()
        first_receipt = self.receipt(package)
        qualified = first_receipt.read_text()
        self.assertTrue(
            qualified.startswith(built),
            "Q5 the qualification receipt must open with the build receipt verbatim",
        )
        self.assertNotEqual(qualified, built, "Q5 the receipt must add the judgement")
        self.assertIn("qualification_sha256=", qualified)
        self.assertNotIn("qualification_sha256=", built, "Q1 the build fact is procedure-free")

        # Q1: rewrite a comment in the procedure. The package must be the same
        # directory, its build receipt must still hold, and cmake must not run.
        script = self.scripts / "bootstrap_kani.sh"
        script.write_text(script.read_text().replace(
            "#!/usr/bin/env bash\n", "#!/usr/bin/env bash\n# an edit that changes no artifact\n", 1))
        self.calls.write_text("")
        self.assertEqual(self.run_bootstrap().returncode, 0)
        self.assertEqual(self.backend(), package, "Q1 the package must not be renamed")
        rebuilt_tools = [line for line in self.calls.read_text().splitlines()
                         if line.startswith("cmake")]
        self.assertEqual(rebuilt_tools, [], "Q1 an edited procedure must not rebuild")
        self.assertEqual((package / "built.txt").read_text(), built, "Q1 the build fact is unchanged")
        second_receipt = self.receipt(package)
        self.assertNotEqual(second_receipt, first_receipt, "Q1 a changed procedure owns its own receipt")
        self.assertNotEqual(
            second_receipt.read_text(), qualified,
            "Q1 a changed procedure must produce a new judgement",
        )
        # Q6: the earlier procedure's judgement survives; a worktree still on
        # that revision of the script must not have to re-qualify.
        self.assertEqual(first_receipt.read_text(), qualified, "Q6 another procedure's receipt is untouched")

        # Q3: an untouched tree reuses the package and still does not rebuild.
        self.calls.write_text("")
        self.assertEqual(self.run_bootstrap().returncode, 0)
        reused_tools = [line for line in self.calls.read_text().splitlines()
                        if line.startswith("cmake")]
        self.assertEqual(reused_tools, [], "Q3 an unchanged package is reused")

        # Q4: replace a published binary. The receipt no longer describes these
        # bytes, so they are not reused. Refusing instead would wedge the
        # package against every later attempt, which is the failure a
        # half-finished earlier run leaves behind; the answer is to rebuild from
        # the reviewed source, and the substituted bytes must not survive it.
        self.receipt(package).unlink()
        substituted = "#!/bin/sh\nexit 0\n"
        (package / "bin/cbmc").write_text(substituted)
        self.calls.write_text("")
        rebuilt = self.run_bootstrap()
        self.assertEqual(rebuilt.returncode, 0, rebuilt.stderr)
        self.assertIn("no longer matches its build receipt", rebuilt.stderr)
        self.assertIn("cmake --build", self.calls.read_text(), "Q4 a mismatch must rebuild")
        self.assertNotEqual(
            (package / "bin/cbmc").read_text(), substituted,
            "Q4 the substituted bytes must not survive the rebuild",
        )
        self.assertTrue(
            self.receipt(package).read_text().startswith((package / "built.txt").read_text()),
            "Q4 the new judgement names the newly built bytes",
        )

    def test_a_changed_artifact_input_builds_a_different_package(self):
        """Q2: the inputs that decide the bytes still name the cache directory."""

        self.assertEqual(self.run_bootstrap().returncode, 0)
        first = self.backend()
        script = self.scripts / "bootstrap_kani.sh"
        source = script.read_text()
        revision = re.search(r'readonly cbmc_revision="([^"]+)"', source)[1]
        replacement = "0" * len(revision)
        script.write_text(source.replace(revision, replacement))
        self.calls.write_text("")
        result = self.run_bootstrap(CBMC_PINNED_REVISION=replacement)
        self.assertEqual(result.returncode, 0, result.stderr)
        packages = sorted(path.name for path in self.cache.glob("cbmc-*"))
        self.assertEqual(len(packages), 2, f"Q2 a new artifact needs its own package: {packages}")
        self.assertIn("cmake", self.calls.read_text(), "Q2 a changed artifact input must rebuild")
        self.assertTrue(any(first.glob("qualified-*.txt")), "Q2 the previous package is untouched")


@unittest.skipIf(os.name == "nt", "POSIX tool-source admission is qualified in the Linux gate")
class ActualPatchSourceTests(unittest.TestCase):
    def test_reviewed_new_file_is_exactly_admitted_without_widening_source_gates(self):
        # Real-Git decision table (the mock Git build port cannot own these):
        # G1 clean base + reviewed tracked/new files -> apply, ITA new file,
        #    no cached delta, exact complete source accepted.
        # G2 replay -> same bytes accepted without reapplying.
        # G3 arbitrary untracked, G4 staged source, G5 unreviewed ITA -> refuse.
        # Both tool call sites execute this one production apply/verifier owner.
        source = (ROOT / "scripts/ci/bootstrap_kani.sh").read_text()
        indented = "\n".join("  " + line for line in source.splitlines())
        functions = "\n".join(nested_shell_function(indented, name)
                              for name in ("verify_source", "apply_reviewed_patch"))
        self.assertIn('apply_reviewed_patch CBMC "$cbmc_source"', source)
        self.assertIn('apply_reviewed_patch Kani "$source_dir"', source)
        environment = detached_git_environment()
        with tempfile.TemporaryDirectory(prefix="awaken-real-patch-") as directory:
            outer = Path(directory)
            repo = outer / "source"
            repo.mkdir()

            def git(*arguments, check=True):
                return subprocess.run(["git", "-c", "core.hooksPath=/dev/null",
                                       "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                       "-c", "commit.gpgsign=false", *arguments],
                                      cwd=repo, env=environment, capture_output=True, text=True, check=check)

            git("init", "--quiet")
            tracked = repo / "tracked.txt"
            added = repo / "new source\tmodule.rs"
            tracked.write_text("before\n")
            git("add", "tracked.txt")
            git("commit", "--quiet", "-m", "fixture base")
            revision = git("rev-parse", "HEAD").stdout.strip()
            tracked.write_text("after\n")
            added.write_text("reviewed new source\n")
            git("add", "--intent-to-add", added.name)
            patch = outer / "reviewed.patch"
            patch.write_text(git("diff", "--binary", "HEAD").stdout)
            git("restore", "--source=HEAD", "--staged", "--worktree", "tracked.txt")
            git("reset", "--quiet", "HEAD", "--", added.name)
            added.unlink()

            def apply():
                script = ("set -euo pipefail\n" + functions + "\napply_reviewed_patch Kani "
                          + " ".join(shlex.quote(str(v)) for v in (repo, revision, patch)) + "\n")
                return run_shell_fixture([bash_executable(), "-c", script], env=environment,
                                         capture_output=True, text=True)

            result = apply()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(added.read_text(), "reviewed new source\n", "G1")
            self.assertEqual(git("ls-files", "--others", "--exclude-standard").stdout, "", "G1")
            self.assertEqual(git("diff", "--cached", "--quiet", check=False).returncode, 0, "G1 no --index")
            self.assertEqual(apply().returncode, 0, "G2")
            unexpected = repo / "unexpected.txt"
            unexpected.write_text("foreign input\n")
            self.assertNotEqual(apply().returncode, 0, "G3")
            unexpected.unlink()
            git("add", "tracked.txt")
            self.assertNotEqual(apply().returncode, 0, "G4")
            git("reset", "--quiet", "HEAD", "--", "tracked.txt")
            unexpected.write_text("unreviewed indexed input\n")
            git("add", "--intent-to-add", "unexpected.txt")
            self.assertNotEqual(apply().returncode, 0, "G5")


if __name__ == "__main__":
    strict_unittest.main()
