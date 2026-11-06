"""SDK consumer qualification IO and compiler-input cases, loaded by
`check_sdk_consumer.py --self-test` from the running checker module."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


def shared_sdk_io_tests(owner):
    """Use the running SDK owner; never import it as a second module instance."""
    class SharedSdkIOTests(unittest.TestCase):
        def setUp(self):
            from _build_observation_fixture import synthetic_build_observations
            synthetic_build_observations(self)

        @staticmethod
        def facts(*, dirty=False):
            return {"revision": b"a" * 40 + b"\n", "tree": b"b" * 40 + b"\n",
                    "index_tree": b"b" * 40 + b"\n", "status": b" M tracked.txt\n" if dirty else b"",
                    "staged": b"", "working": b"changed" if dirty else b"", "untracked": b""}

        def test_stdio_adapter_keeps_split_merged_status_and_spawn_observations(self):
            # D1 split bare capture vs D2 merged qualification capture: identical
            # argv/env/cwd/check and original0/23/75/signal remain unchanged; the
            # returned CompletedProcess is the original object. D3 spawn failure
            # propagates once, with no retry or synthetic successful observation.
            command = ["cargo", "check", "--literal", "sdk"]
            arguments = {"cwd": Path("fixture"), "env": {"SDK_IO": "fixture"}, "check": False}
            for merged in (False, True):
                stdio = ({"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT}
                         if merged else {"capture_output": True})
                for raw in (0, 23, 75, -15):
                    completed = subprocess.CompletedProcess(command, raw, "original output\n", None if merged else "original diagnostic\n")
                    with self.subTest(merged=merged, raw=raw), mock.patch.object(owner.subprocess, "run", return_value=completed) as run:
                        observed = owner.capture_sdk_command(command, merged=merged, **arguments)
                        self.assertIs(observed, completed)
                        self.assertEqual(observed.returncode, raw)
                        self.assertEqual(observed.stderr, None if merged else "original diagnostic\n")
                        run.assert_called_once_with(command, text=True, **stdio, **arguments)
                with mock.patch.object(owner.subprocess, "run", side_effect=OSError("original spawn refusal")) as run:
                    with self.assertRaisesRegex(OSError, "original spawn refusal"):
                        owner.capture_sdk_command(command, merged=merged, **arguments)
                    run.assert_called_once()

        def test_metadata_json_keeps_diagnostics_separate_and_allows_the_original_successors(self):
            # D9 qualification asks for merged diagnostics, but Cargo metadata
            # owns a stdout JSON protocol. Valid JSON + normal stderr warnings
            # => the existing sequence reaches all original eight tasks and its
            # sample command. D10 bad stdout JSON ->1 before either successor;
            # D11 child75 remains75. This is controlled IO, not artifact or SDK
            # execution evidence. No prefix stripping or --quiet is involved.
            warning = "    Updating crates.io index\nwarning: patch fixture was not used\n"
            tasks = "\n".join(f"test {name} ... ok" for name in sorted(owner.EXPECTED))
            tasks += f"\ntest result: ok. {len(owner.EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
            for cargo in (["cargo"], ["cargo", "+1.96.0"], [str(Path("tools/cargo.cmd"))]):
                for bad_json, raw, expected in ((False, 0, 0), (True, 0, 1), (False, 75, 75)):
                    with self.subTest(cargo=cargo, bad_json=bad_json, raw=raw), tempfile.TemporaryDirectory() as temporary:
                        root = Path(temporary)
                        command = [*cargo, "metadata", "--format-version", "1"]
                        sequence = [command,
                            ["cargo", "test", "--test", "tasks", "--", "--test-threads=1"],
                            ["cargo", "run"]]
                        calls, completed = [], None
                        metadata = "bad-json" if bad_json else json.dumps({"workspace_root": str(root)})
                        def spawn(actual, **arguments):
                            calls.append(actual)
                            if actual == command:
                                merged = arguments.get("stderr") == subprocess.STDOUT
                                return subprocess.CompletedProcess(actual, raw,
                                    warning + metadata if merged else metadata, None if merged else warning)
                            return subprocess.CompletedProcess(actual, 0, tasks if actual[1] == "test" else "sample output\n", None)
                        def capture(actual, **arguments):
                            nonlocal completed
                            completed = owner.capture_sdk_command(actual, merged=True, **arguments)
                            return completed
                        def record(rows):
                            label = str(len(rows))
                            owner.retain_sdk_command_output(root, label, completed)
                            if rows[-1]["exit_code"]:
                                return
                            if rows[-1]["command"] == command:
                                self.assertEqual(json.loads(completed.stdout)["workspace_root"], str(root))
                            elif rows[-1]["command"][1] == "test":
                                owner.validate_task_evidence(completed.stdout)
                        with mock.patch.object(owner.subprocess, "run", side_effect=spawn), \
                                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                            status, rows = owner.crate_test.execute_commands(sequence, {}, root=root, run=capture, record=record)
                        self.assertEqual(status, expected)
                        self.assertEqual(calls, sequence if expected == 0 else [command])
                        self.assertEqual(rows[0]["exit_code"], raw)
                        self.assertEqual((root / "1.stdout.log").read_text(), metadata)
                        self.assertEqual((root / "1.stderr.log").read_text(), warning)
                        self.assertEqual((root / "1.log").read_text(), metadata + warning)

            # D12 non-metadata qualification subjects keep merged IO; bare
            # subjects keep split IO. A later literal "metadata" argument must
            # not be mistaken for Cargo's subcommand.
            for command in (["cargo", "semver-checks", "check-release"],
                            ["cargo", "+1.96.0", "package"],
                            ["cargo", "test", "metadata"], ["python3", "metadata"]):
                for merged in (False, True):
                    with self.subTest(command=command, merged=merged), mock.patch.object(owner.subprocess, "run") as run:
                        owner.capture_sdk_command(command, merged=merged)
                        self.assertEqual(run.call_args.kwargs.get("stderr"), subprocess.STDOUT if merged else None)
                        self.assertEqual(run.call_args.kwargs.get("capture_output"), None if merged else True)

        def test_shared_observation_keeps_bare_wip_and_qualification_clean_policy(self):
            # D4 the renderer only observes original facts. D5 initial WIP is
            # allowed by the ordinary consumer, while D6 package/semver require
            # clean source before tools or metadata; moving the renderer must
            # not merge those different policies. Original drift/failure rows
            # remain covered by the owning SDK suite, loaded only once.
            facts = self.facts(dirty=True)
            with mock.patch.object(owner.rust_test_plan, "working_subject", return_value=("original-subject", facts)):
                observed, raw, rendered = owner.sdk_source_observation()
                self.assertEqual(observed, "original-subject")
                self.assertIs(raw, facts)
                self.assertEqual(rendered, {"revision": "a" * 40, "tree": "b" * 40,
                    "index_tree": "b" * 40, "working_subject": "original-subject"})
            helper = owner.SdkConsumerTests()  # Reuse fixture methods, not its runnable suite.
            for qualification in ("", "package", "semver"):
                with self.subTest(qualification=qualification), tempfile.TemporaryDirectory() as temporary:
                    root = helper.repository(Path(temporary), successor=True)
                    with mock.patch.object(owner.rust_test_plan, "working_subject", return_value=("original-subject", facts)), \
                            mock.patch.object(owner, "sdk_source_observation", wraps=owner.sdk_source_observation) as observe:
                        status, calls = helper.execute(root, qualification=qualification)
                    self.assertEqual(status, 1 if qualification else 0)
                    self.assertEqual(len(calls), 0 if qualification else 2)
                    self.assertGreater(observe.call_count, 0)

        def test_retained_channels_preserve_captured_utf8_bytes_and_hash(self):
            # B1 LF, B2 existing CRLF, B3 mixed Unicode/channels, B4 empty:
            # retained bytes must match the UTF-8 bytes bound by log_sha256.
            # Platform newline conversion must never revoke a valid witness.
            for stdout, stderr in (("first\nsecond\n", "diagnostic\n"),
                                   ("first\r\n", "diagnostic\r\n"),
                                   ("λ\n汉字\r\n", "é\r\nlast\n"), ("", "")):
                with self.subTest(stdout=stdout, stderr=stderr), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    completed = subprocess.CompletedProcess(["fixture"], 0, stdout, stderr)
                    owner.retain_sdk_command_output(directory, "captured", completed)
                    self.assertEqual((directory / "captured.stdout.log").read_bytes(), stdout.encode("utf-8"))
                    self.assertEqual((directory / "captured.stderr.log").read_bytes(), stderr.encode("utf-8"))
                    combined = (directory / "captured.log").read_bytes()
                    self.assertEqual(combined, (stdout + stderr).encode("utf-8"))
                    self.assertEqual(hashlib.sha256(combined).hexdigest(),
                                     hashlib.sha256((stdout + stderr).encode("utf-8")).hexdigest())


    return unittest.defaultTestLoader.loadTestsFromTestCase(SharedSdkIOTests)


def compiler_input_tests(owner):
    """Compose the original fixture without loading its common suite again."""
    class SdkCompilerInputTests(unittest.TestCase):
        @contextlib.contextmanager
        def directory_alias(self, root, alias, target):
            # Both endpoints belong to this private fixture. A Windows junction
            # exercises the same filesystem alias without symlink privilege.
            root, target = root.resolve(strict=True), target.resolve(strict=True)
            self.assertTrue(target.is_relative_to(root))
            self.assertTrue(alias.parent.resolve(strict=True).is_relative_to(root))
            self.assertFalse(os.path.lexists(alias))
            if os.name == "nt":
                import _winapi
                _winapi.CreateJunction(str(target), str(alias))
            else:
                alias.symlink_to(target, target_is_directory=True)
            try:
                self.assertEqual(alias.resolve(strict=True), target)
                yield
            finally:
                self.assertEqual(alias.resolve(strict=True), target)
                if os.name == "nt":
                    self.assertTrue(alias.is_junction())
                    alias.rmdir()
                else:
                    self.assertTrue(alias.is_symlink())
                    alias.unlink()

        @contextlib.contextmanager
        def subject(self, *, cause="", baseline_model=None, current_k8s=True):
            with tempfile.TemporaryDirectory() as temporary:
                helper = owner.SdkConsumerTests()
                root = helper.repository(Path(temporary))
                if baseline_model is not None:
                    helper.compiler_source(root, sdk_k8s=True, model=baseline_model, version="0.28.0")
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private baseline compiler model")
                model = {"missing": None, "multiple": ["v1_32", "v1_33"], "invalid": "latest"}.get(cause, "v1_32")
                helper.compiler_source(root, sdk_k8s=current_k8s, model=model, version="0.28.0")
                (root / "tracked.txt").write_text("current compiler input subject\n")
                if cause == "unknown-dependency":
                    path = root / "crates/fixture-provider/Cargo.toml"
                    path.write_text(path.read_text() + 'unrecorded="1"\n')
                elif cause == "unknown-workspace-dependency":
                    path = root / "crates/fixture-provider/Cargo.toml"
                    path.write_text(path.read_text() + 'unrecorded={workspace=true}\n')
                elif cause in ("renamed-target", "inherited-target"):
                    path = root / "crates/fixture-provider/Cargo.toml"
                    dependency = 'kube_client={package="kube",version="4.2.0"}\n'
                    if cause == "inherited-target":
                        workspace = root / "Cargo.toml"
                        workspace.write_text(workspace.read_text() + dependency)
                        dependency = 'kube_client={workspace=true}\n'
                    path.write_text(path.read_text().replace('kube={version="4.2.0",optional=true}\n', '')
                                    + '[target.\'cfg(windows)\'.build-dependencies]\n' + dependency)
                elif cause == "root-malformed":
                    (root / "Cargo.toml").write_text("[workspace\n")
                elif cause == "root-absent":
                    (root / "Cargo.toml").unlink()
                helper.git(root, "add", ".")
                helper.git(root, "commit", "--quiet", "-m", "private current compiler model")
                with mock.patch.dict(os.environ):
                    os.environ.pop("K8S_OPENAPI_ENABLED_VERSION", None)
                    yield helper, root

        @staticmethod
        def compiles(calls):
            return [command for command in calls if command[:2] in
                    (["cargo", "+1.96.0"], ["cargo", "semver-checks"])]

        def test_explicit_input_reaches_original_profiles_without_changing_ambient_or_baseline(self):
            # K1 model=v1_32 + supported unique registry identity + ambient
            # absent/equal => derive1.32 only in package/SemVer child values;
            # retain both original profile argv and SDK selection. K2 baseline
            # SDK normal/build closure has no K8s despite Platform v1_30 =>
            # baseline model=null/not_applicable, shared input inherited-unused.
            for mode in ("package", "semver"):
                for ambient in (None, "1.32"):
                    with self.subTest(mode=mode, ambient=ambient), self.subject() as (helper, root):
                        if ambient is not None:
                            os.environ["K8S_OPENAPI_ENABLED_VERSION"] = ambient
                        observations = []
                        status, calls = helper.execute(root, qualification=mode,
                            compiler_cause="valid", compiler_observations=observations)
                        self.assertEqual(status, 0)
                        self.assertEqual(len(self.compiles(calls)), 2)
                        self.assertEqual([row["compiler_input"] for row in observations], ["1.32", "1.32"])
                        self.assertEqual([row["ambient"] for row in observations], [ambient, ambient])
                        self.assertEqual(os.environ.get("K8S_OPENAPI_ENABLED_VERSION"), ambient)
                        path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                        receipt = json.loads(path.read_text())["compiler_inputs"]
                        self.assertEqual(receipt["environment"], {"K8S_OPENAPI_ENABLED_VERSION": "1.32"})
                        self.assertEqual(receipt["ambient"], "absent" if ambient is None else "matching")
                        self.assertEqual(receipt["current"]["model"]["feature"], "v1_32")
                        self.assertEqual(receipt["current"]["model"]["package"]["version"], "0.28.0")
                        if mode == "semver":
                            self.assertEqual(receipt["baseline"]["applicability"], "not_applicable")
                            self.assertIsNone(receipt["baseline"]["model"])
                            self.assertEqual(receipt["baseline"]["shared_environment"], "inherited_unused")

        def test_invalid_model_identity_graph_or_ambient_starts_no_compiler(self):
            # K3 empty/conflicting ambient; K4 absent/non-explicit/multiple or
            # unsupported model; K5 inconsistent/duplicate registry identity,
            # incomplete metadata or unresolved normal/build source edge =>
            # refusal before either compiler. No all-feature narrowing or SDK
            # library feature edit can substitute for this preflight evidence.
            causes = ("missing", "multiple", "invalid", "root-malformed", "unsupported", "identity",
                      "version", "duplicate", "incomplete", "unknown-dependency", "unknown-workspace-dependency",
                      "ambient-empty", "ambient-conflict")
            for mode in ("package", "semver"):
                for cause in causes:
                    with self.subTest(mode=mode, cause=cause), self.subject(cause=cause) as (helper, root):
                        if cause.startswith("ambient-"):
                            os.environ["K8S_OPENAPI_ENABLED_VERSION"] = "" if cause == "ambient-empty" else "1.33"
                        status, calls = helper.execute(root, qualification=mode, compiler_cause=cause)
                        self.assertEqual(status, 1)
                        self.assertEqual(self.compiles(calls), [])

        def test_renamed_target_edges_use_the_exact_git_workspace_authority(self):
            # K10 explicit/inherited aliases on a foreign-target build edge
            # retain the actual locked package in both qualifier modes. Source
            # observation must use the selected Git revision's workspace table,
            # never a live Root manifest. Original compiler profiles stay exact.
            import _crate_boundary_workspace
            original = owner._sdk_compiler_input.observe_source
            load = _crate_boundary_workspace.load_manifest
            def observe(*arguments, **keywords):
                def archived_only(path):
                    if Path(path).resolve().is_relative_to(root.resolve()):
                        raise AssertionError("live workspace read")
                    return load(path)
                with mock.patch.object(_crate_boundary_workspace, "load_manifest",
                                       side_effect=archived_only):
                    return original(*arguments, **keywords)
            for mode in ("package", "semver"):
                for cause in ("renamed-target", "inherited-target"):
                    with self.subTest(mode=mode, cause=cause), self.subject(cause=cause) as (helper, root):
                        with mock.patch.object(owner._sdk_compiler_input, "observe_source", side_effect=observe):
                            status, calls = helper.execute(root, qualification=mode, compiler_cause="valid")
                        self.assertEqual((status, len(self.compiles(calls))), (0, 2))

        def test_applicable_sides_with_conflicting_models_refuse_without_overriding_baseline(self):
            # K6 baseline SDK actually reaches K8s and declaresv1_33 while the
            # current SDK declaresv1_32 => one shared compiler environment cannot
            # represent both; refuse before tool builds, do not borrow current.
            with self.subject(baseline_model="v1_33") as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (1, []))

            # The compatible branch requires both independent source/lock model
            # declarations, not a value copied from current. A current SDK that
            # no longer reaches K8s also cannot silently borrow baseline policy.
            with self.subject(baseline_model="v1_32") as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, len(self.compiles(calls))), (0, 2))
                path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                baseline = json.loads(path.read_text())["compiler_inputs"]["baseline"]
                self.assertEqual(baseline["model"]["value"], "1.32")
                self.assertEqual(baseline["shared_environment"], "model_used")
            with self.subject(baseline_model="v1_32", current_k8s=False) as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (1, []))

        def test_baseline_archive_preserves_git_bytes_under_ambient_line_endings(self):
            # B14 Windows checkout policy is not an authored compiler change.
            # The same historical Git subject must yield identical compiler
            # hashes and archive bytes under all three autocrlf modes, without
            # rewriting any local Git setting. B15 below retains the refusal
            # when an authored attribute really transforms compiler evidence.
            with tempfile.TemporaryDirectory() as temporary:
                helper = owner.SdkConsumerTests()
                root = helper.repository(Path(temporary))
                revision = owner.formal_sources.git_output(root, "rev-parse", "HEAD").decode().strip()
                archives = set()
                for autocrlf in ("true", "input", "false"):
                    with self.subTest(autocrlf=autocrlf):
                        helper.git(root, "config", "core.autocrlf", autocrlf)
                        helper.git(root, "config", "core.eol", "crlf")
                        inventory = owner._sdk_compiler_input.baseline_inventory(root, revision)
                        self.assertEqual(inventory["sdk_crates"], ["fixture-contract", "fixture-provider"])
                        for path in ("Cargo.toml", "Cargo.lock", "crates/fixture-provider/Cargo.toml"):
                            original = owner.formal_sources.git_output(root, "show", revision + ":" + path)
                            self.assertEqual(inventory["source_hashes"][path], hashlib.sha256(original).hexdigest())
                        archives.add(inventory["archive_sha256"])
                        for key, expected in (("core.autocrlf", autocrlf), ("core.eol", "crlf")):
                            self.assertEqual(owner.formal_sources.git_output(root, "config", "--local", "--get", key)
                                             .decode().strip(), expected)
                self.assertEqual(len(archives), 1)

        def test_baseline_inventory_tracks_existing_package_names_across_historical_paths(self):
            # B1 baseline has the same SDK package at a different directory;
            # current metadata names its current directory. Effect: resolve
            # historical manifest/dependency/model facts by baseline's own
            # complete workspace inventory, never classify the moved package
            # as first release or omit it from either original SemVer profile.
            with tempfile.TemporaryDirectory() as temporary:
                helper = owner.SdkConsumerTests()
                root = helper.repository(Path(temporary))
                original = root / "crates/fixture-provider"
                historical = root / "crates/historical-provider"
                original.rename(historical)
                helper.git(root, "add", ".")
                helper.git(root, "commit", "--quiet", "-m", "private historical package location")
                baseline = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                    env=owner.formal_sources.detached_git_environment(), text=True).strip()
                historical.rename(original)
                (root / "tracked.txt").write_text("current moved SDK\n")
                helper.git(root, "add", ".")
                helper.git(root, "commit", "--quiet", "-m", "private moved package")
                status, calls = helper.execute(root, qualification="semver", baseline_rev=baseline)
                self.assertEqual((status, len(self.compiles(calls))), (0, 2))
                path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                receipt = json.loads(path.read_text())
                self.assertEqual(receipt["semver_crates"], ["fixture-contract", "fixture-provider"])
                self.assertEqual(receipt["first_release_crates"], [])
                inventory = receipt["compiler_inputs"]["baseline_inventory"]
                self.assertEqual(inventory["members"]["fixture-provider"]["manifest_path"],
                                 "crates/historical-provider/Cargo.toml")

        def test_first_release_packages_remain_packaged_but_do_not_claim_backward_semver(self):
            # B2 complete valid baseline lacks these package names in its whole
            # workspace; current has two new SDK leaves. Effect: package keeps
            # the full four-member current selector; SemVer keeps both real
            # historical packages in default/all, with explicit first-release
            # N/A rather than fabricated compatibility. Both original profile
            # commands still run and retain exact source/record validation.
            for mode in ("package", "semver"):
                with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary:
                    helper = owner.SdkConsumerTests()
                    root = helper.repository(Path(temporary))
                    for name in ("fixture-new-sync", "fixture-new-windows"):
                        manifest = root / "crates" / name / "Cargo.toml"
                        manifest.parent.mkdir()
                        manifest.write_text('[package]\nname=' + json.dumps(name)
                            + '\nversion="1.0.0"\n[package.metadata.awaken]\nproduct="sdk"\napi_snapshot=true\n')
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private first release SDK leaves")
                    status, calls = helper.execute(root, qualification=mode, metadata_cause="first-release")
                    self.assertEqual((status, len(self.compiles(calls))), (0, 2))
                    path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                    receipt = json.loads(path.read_text())
                    self.assertEqual(len(receipt["sdk_crates"]), 4)
                    for command in self.compiles(calls):
                        names = [command[index + 1] for index, word in enumerate(command) if word == "-p"]
                        self.assertEqual(len(names), 4 if mode == "package" else 2)
                    if mode == "semver":
                        self.assertEqual(receipt["first_release_crates"],
                                         ["fixture-new-sync", "fixture-new-windows"])
                        self.assertEqual(receipt["semver_crates"], ["fixture-contract", "fixture-provider"])

        def test_invalid_or_colliding_baseline_inventory_never_becomes_first_release(self):
            # B3 duplicate package name; B4 missing explicitly declared member;
            # B5 malformed historical manifest; B6 historical package was not
            # SDK but reuses a current SDK name; B7 a historical SDK disappears;
            # B8 member path escapes the archive. Every row refuses before any
            # compiler. The valid moved/current-only positives above prevent
            # a blanket refusal from satisfying this table. Baseline model
            # conflict and original raw75/101/143 precedence remain covered by
            # their original adjacent tests, not a separate failure classifier.
            for cause in ("collision", "missing-member", "malformed", "changed-publication",
                          "retired-sdk", "escaping-member"):
                with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                    helper = owner.SdkConsumerTests()
                    root = helper.repository(Path(temporary))
                    manifest = root / "crates/fixture-provider/Cargo.toml"
                    original = manifest.read_text()
                    if cause in ("collision", "retired-sdk"):
                        duplicate = root / "crates/duplicate/Cargo.toml"
                        duplicate.parent.mkdir()
                        duplicate.write_text(original if cause == "collision" else
                            original.replace('name="fixture-provider"', 'name="fixture-retired"'))
                    elif cause in ("missing-member", "escaping-member"):
                        workspace = root / "Cargo.toml"
                        workspace.write_text(workspace.read_text().replace('members=["crates/*"]',
                            'members=["crates/fixture-contract","crates/fixture-provider",'
                            + json.dumps("crates/missing" if cause == "missing-member" else "../outside")
                            + ',"crates/fixture-internal"]'))
                    elif cause == "malformed":
                        manifest.write_text("[package\n")
                    else:
                        manifest.write_text(original.replace('product="sdk"', 'product="platform"'))
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private invalid historical inventory")
                    baseline = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                        env=owner.formal_sources.detached_git_environment(), text=True).strip()
                    if cause in ("collision", "retired-sdk"):
                        (root / "crates/duplicate/Cargo.toml").unlink()
                        (root / "crates/duplicate").rmdir()
                    helper.compiler_source(root)
                    (root / "tracked.txt").write_text("restored current SDK\n")
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private valid current inventory")
                    status, calls = helper.execute(root, qualification="semver", baseline_rev=baseline)
                    self.assertEqual((status, self.compiles(calls)), (1, []))

        def test_archive_attributes_cannot_hide_or_rewrite_historical_sdk_evidence(self):
            # B9 tracked export-ignore can omit one existing SDK while the
            # wildcard still finds other packages; B10 local info/attributes
            # can do the same without changing the Git tree; B11 export-subst
            # can rewrite a participating manifest or lock in an archive.
            # Effects: the existing historical manifest remains provably in
            # Git, but every incomplete/rewritten archive refuses BEFORE any
            # compiler, never classifies it as first release. The original
            # complete/moved/true-new positives above prevent blanket refusal.
            for cause in ("export-ignore", "info-ignore", "info-ignore-lock",
                          "export-subst-manifest", "export-subst-lock", "info-subst-manifest", "eol-crlf"):
                with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                    helper = owner.SdkConsumerTests()
                    root = helper.repository(Path(temporary))
                    target = "Cargo.lock" if "lock" in cause else "crates/fixture-provider/Cargo.toml"
                    if "subst" in cause:
                        path = root / target
                        path.write_text(path.read_text() + "# $Format:%H$\n")
                    attribute = ("Cargo.lock" if "lock" in cause else
                                 "crates/fixture-provider" if "ignore" in cause else target)
                    content = attribute + (" export-ignore\n" if "ignore" in cause else " export-subst\n")
                    # B15 explicit authored CRLF conversion remains a byte
                    # rewrite; do not normalize away the integrity failure.
                    if cause == "eol-crlf":
                        content = attribute + " text eol=crlf\n"
                    if not cause.startswith("info-"):
                        (root / ".gitattributes").write_text(content)
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private historical archive attributes",
                               "--allow-empty")
                    baseline = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                        env=owner.formal_sources.detached_git_environment(), text=True).strip()
                    if cause.startswith("info-"):
                        info = root / ".git/info/attributes"
                        info.parent.mkdir(exist_ok=True)
                        info.write_text(content)
                    (root / "tracked.txt").write_text("current SDK after historical archive attributes\n")
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private current SDK")
                    historical = owner.formal_sources.git_output(root, "show",
                        baseline + ":crates/fixture-provider/Cargo.toml")
                    self.assertIn(b'name="fixture-provider"', historical)
                    self.assertIn(b'product="sdk"', historical)
                    status, calls = helper.execute(root, qualification="semver", baseline_rev=baseline)
                    self.assertEqual((status, self.compiles(calls)), (1, []))

        def test_git_subject_rejects_gitlinks_and_ignores_unselected_current_path_aliases(self):
            # B12 an unexpanded historical gitlink cannot prove an absent SDK:
            # refuse before compiler, rather than classify it as first release.
            # B13 historical package moved and its old directory is reused by
            # a current, unselected directory alias: read the original relative Git path,
            # not that live filesystem alias. Current metadata still obeys its
            # original absolute/resolve/root gate; historical ../ and absolute
            # Git paths refuse through the same read owner, with no fallback.
            for cause in ("gitlink", "historical-alias"):
                with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary, contextlib.ExitStack() as aliases:
                    helper = owner.SdkConsumerTests()
                    root = helper.repository(Path(temporary))
                    original = root / "crates/fixture-provider"
                    historical = root / "crates/historical-provider"
                    if cause == "gitlink":
                        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                            env=owner.formal_sources.detached_git_environment(), text=True).strip()
                        helper.git(root, "rm", "-r", "crates/fixture-provider")
                        helper.git(root, "update-index", "--add", "--cacheinfo",
                                   "160000," + commit + ",crates/fixture-provider")
                    else:
                        original.rename(historical)
                        helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private historical Git subject")
                    baseline = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                        env=owner.formal_sources.detached_git_environment(), text=True).strip()
                    if cause == "gitlink":
                        self.assertEqual(owner.formal_sources.git_paths(root, "ls-tree", "-z",
                            "--format=%(objecttype)", baseline, "--", "crates/fixture-provider"), ["commit"])
                        helper.git(root, "rm", "--cached", "crates/fixture-provider")
                        helper.compiler_source(root)
                    else:
                        historical.rename(original)
                        aliases.enter_context(self.directory_alias(root, historical, root / "crates/fixture-internal"))
                        workspace = root / "Cargo.toml"
                        workspace.write_text(workspace.read_text().replace('members=["crates/*"]',
                            'members=["crates/fixture-contract","crates/fixture-provider","crates/fixture-internal"]'))
                    (root / "tracked.txt").write_text("current SDK with separate filesystem paths\n")
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private current Git subject")
                    status, calls = helper.execute(root, qualification="semver", baseline_rev=baseline)
                    self.assertEqual((status, len(self.compiles(calls))),
                                     (1, 0) if cause == "gitlink" else (0, 2))
                    if cause == "historical-alias":
                        for path in ("../outside/Cargo.toml", "/outside/Cargo.toml"):
                            with self.subTest(historical_escape=path), self.assertRaises(ValueError):
                                owner._sdk_compiler_input.observe_source(root, baseline, {}, ["fixture-provider"],
                                    git_manifest_paths={"fixture-provider": path})
                        with self.assertRaises(ValueError):
                            owner._sdk_compiler_input.observe_source(root, baseline,
                                {"fixture-provider": {"manifest_path": str(root.parent / "outside/Cargo.toml")}},
                                ["fixture-provider"])

        def test_unreadable_inputs_and_post_child_faults_preserve_first_status(self):
            # K7 a Git source read failure retains its original status (75) or
            # IO refusal1 before any compiler. K8 child0/75/101/signal143 crossed
            # with post-source/log/journal uncertainty =>0 becomes1, original
            # nonzero wins and no second profile starts. Existing record/first
            # status owner, not the compiler-input policy, owns these outcomes.
            for error, expected in ((OSError("compiler input unreadable"), 1),
                                    (subprocess.CalledProcessError(75, ["git", "show"]), 75)):
                with self.subTest(read=expected), self.subject() as (helper, root):
                    original = owner.formal_sources.git_output
                    def read(subject, *arguments):
                        if arguments[0] == "show" and arguments[-1].endswith(":Cargo.toml"):
                            raise error
                        return original(subject, *arguments)
                    with mock.patch.object(owner.formal_sources, "git_output", side_effect=read):
                        status, calls = helper.execute(root, qualification="package", compiler_cause="valid")
                    self.assertEqual((status, self.compiles(calls)), (expected, []))
            with self.subject(cause="root-absent") as (helper, root):
                status, calls = helper.execute(root, qualification="package", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (128, []))
            for raw in (0, 75, 101, -15):
                for cause in ("source", "log", "journal"):
                    with self.subTest(raw=raw, cause=cause), self.subject() as (helper, root):
                        arguments = {"mutation": "working"} if cause == "source" else {"fault": "output"} if cause == "log" else {}
                        with contextlib.ExitStack() as stack:
                            if cause == "journal":
                                stack.enter_context(mock.patch.object(owner, "retain_sdk_result", side_effect=OSError("compiler input receipt refusal")))
                            status, calls = helper.execute(root, qualification="package", compiler_cause="valid",
                                test_status=raw, phase="package", **arguments)
                        self.assertEqual(status, 143 if raw == -15 else raw or 1)
                        self.assertEqual(len(self.compiles(calls)), 1)

        def test_bare_and_original_artifact_modes_do_not_acquire_the_compiler_policy(self):
            # K9 the ordinary consumer and already-owned default artifact mode
            # retain their original behavior even with conflicting ambient input;
            # this package/SemVer policy adds neither a new prerequisite nor a
            # global sanitizer mutation to those existing paths.
            for mode in ("", "artifact"):
                with self.subTest(mode=mode), self.subject() as (helper, root):
                    os.environ["K8S_OPENAPI_ENABLED_VERSION"] = "conflicting-input"
                    status, _ = helper.execute(root, qualification=mode)
                    self.assertEqual(status, 0)
                    self.assertEqual(os.environ["K8S_OPENAPI_ENABLED_VERSION"], "conflicting-input")
                    if mode:
                        path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                        self.assertNotIn("compiler_inputs", json.loads(path.read_text()))

    return unittest.defaultTestLoader.loadTestsFromTestCase(SdkCompilerInputTests)
