#!/usr/bin/env python3
"""Qualify a required libtest run against its independently listed selection."""
from __future__ import annotations

import argparse
import hashlib
from collections import Counter
import json
import contextlib
import io
import os
from pathlib import Path
import re
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
# The self-test copies the actual transitive CI script closure, including its
# source/deadline owners and production entrypoints, rather than a fixed list.
# Variables a rustup Cargo proxy injects into every child it starts.
PROXY_INJECTED = ("CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "RUSTUP_TOOLCHAIN_SOURCE", "RUST_RECURSION_COUNT")
INPUT_GLOBS = ("scripts/ci/**", "scripts/e2e/sandbox_capability_suite.sh",
               "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "lefthook.yml", ".cargo/**", ".config/**")

LISTED = re.compile(r"^(\S+): test$", re.MULTILINE)
EXECUTED = re.compile(r"^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored)(?:,.*)?$", re.MULTILINE)
SUMMARY = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;", re.MULTILINE)
SKIP = re.compile(r"^[ \t]*(?:skip(?:ped|ping)?\b|\[skip\])", re.MULTILINE | re.IGNORECASE)


def _listed_selection(inventory: str, required: list[str] | tuple[str, ...]) -> tuple[Counter, list[str]]:
    planned = Counter(LISTED.findall(inventory))
    errors: list[str] = []
    if not planned:
        errors.append("required selection is empty")
    missing = sorted(set(required) - set(planned))
    if missing:
        errors.append("required selection omits cases: " + ", ".join(missing))
    return planned, errors


def execution_evidence(inventory: str, results: str, exit_code: int,
                       required: list[str] | tuple[str, ...] = ()) -> dict:
    planned, errors = _listed_selection(inventory, required)
    observed = EXECUTED.findall(results)
    actual = Counter(name for name, _ in observed)
    if exit_code:
        errors.insert(0, f"test command failed with status {exit_code}")
    if actual != planned:
        errors.append(f"selection differs: missing={dict(planned - actual)}, unexpected={dict(actual - planned)}")
    if any(status != "ok" for _, status in observed):
        errors.append("required tests failed or were ignored")
    summaries = SUMMARY.findall(results)
    if not summaries or any(status != "ok" or int(failed) or int(ignored) for status, _, failed, ignored in summaries):
        errors.append("passing, non-skipped execution summaries are required")
    if sum(int(passed) for _, passed, _, _ in summaries) != len(observed):
        errors.append("summary counts differ from named test results")
    if SKIP.search(results):
        errors.append("a selected test skipped its fixture or behavior")
    # The validator, receipt and success count consume this same observation.
    # Independent phase fences still read their own evidence; a second read
    # merely to print a count would qualify one selection and report another.
    return {"errors": errors, "selected_cases": sorted(planned.elements()),
            "executed_cases": observed}


def required_commands(arguments: list[str]) -> tuple[list[list[str]], list[str]]:
    """The existing focused libtest argv, with frontend-only floors removed."""
    if os.environ.get("RUST_TEST_NOCAPTURE") not in (None, "0"):
        raise ValueError("required test execution owns captured output; unset RUST_TEST_NOCAPTURE")
    return selection_commands(arguments)


def selection_commands(arguments: list[str]) -> tuple[list[list[str]], list[str]]:
    """Pure canonical argv/floor projection, independent of the caller's environment."""
    required, cargo_arguments = [], []
    frontend = True
    for argument in arguments:
        if argument == "--":
            frontend = False
        if frontend and argument.startswith("--require-case="):
            case = argument.split("=", 1)[1]
            if not case:
                raise ValueError("required case must be nonempty")
            required.append(case)
        else:
            if argument.split("=", 1)[0] in ("--nocapture", "--no-capture"):
                raise ValueError("required test execution owns captured output; use its --show-output diagnostics")
            if argument == "--list" or argument.split("=", 1)[0] == "--format":
                raise ValueError("required test execution owns list/output format")
            cargo_arguments.append(argument)
    boundary = cargo_arguments.index("--") if "--" in cargo_arguments else len(cargo_arguments)
    locked = [] if "--locked" in cargo_arguments[:boundary] else ["--locked"]
    base = ["cargo", "test", *locked, *cargo_arguments]
    if boundary == len(cargo_arguments):
        base.append("--")
    return [[*base, "--list"], [*base, "--show-output"]], required


def execution_inputs() -> dict:
    """Safe runtime identity, separate from the graph prepared before services start."""
    kind = os.environ.get("AWAKEN_TEST_FIXTURE_KIND")
    owner = os.environ.get("AWAKEN_TEST_FIXTURE_OWNER")
    if (kind is None) != (owner is None):
        raise ValueError("native fixture kind and owner must be supplied together")
    if kind is not None and (kind not in ("postgres", "nats", "s3")
                             or not re.fullmatch(r"[A-Za-z0-9._:-]{1,160}", owner or "")):
        raise ValueError("native fixture subject is malformed")
    backend = os.environ.get("AWAKEN_CRASH_BACKEND") or None
    if backend not in (None, "postgres", "sqlite"):
        raise ValueError("unsupported crash-test backend selector")
    return {"fixture": {"kind": kind, "owner": owner} if kind is not None else None,
            "crash_backend": backend}


def _write_receipt(path: Path, document: dict) -> None:
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def required_build_command(arguments: list[str]) -> list[str]:
    """Compile the authored graph, keeping libtest-only arguments out of Cargo."""
    commands, _ = required_commands(arguments)
    return [*commands[0][:commands[0].index("--")], "--no-run"]


def prepare_required(arguments: list[str], build_log: Path, receipt: Path) -> int:
    """Bound compilation separately; a successful build never qualifies tests."""
    from crate_test import execute_commands, native_command
    from rust_test_plan import source_snapshot, working_subject

    try:
        command = required_build_command(arguments)
    except ValueError as error:
        raise argparse.ArgumentError(None, str(error)) from error
    if build_log.parent != receipt.parent or build_log.exists() or receipt.exists():
        raise ValueError("required build needs one fresh evidence directory")
    digest, facts = working_subject()
    from _required_build_context import build_context
    context = build_context(ROOT, arguments)
    document = {"schema_version": 1, "kind": "required-libtest-build", "full_gate": False,
                "source_kind": "working-tree", "revision": facts["revision"].decode().strip(),
                "source_digest": digest, "index_tree": facts["index_tree"].decode().strip(),
                "arguments": arguments, "command": command, "compile_context": context, "build_log": str(build_log),
                "status": "running", "rows": []}
    _write_receipt(receipt, document)
    def verify():
        if source_snapshot()[0] != digest:
            raise ValueError("Source changed before or during required build")
        build_context(ROOT, arguments, context)
    def run(command, **kwargs):
        with build_log.open("w", encoding="utf-8") as output:
            return subprocess.run(native_command(command, kwargs.get("env")), stdout=output,
                                  stderr=subprocess.STDOUT, **kwargs)
    def record(rows):
        document["rows"] = rows
        _write_receipt(receipt, document)
    status, rows = execute_commands([command], {**os.environ, 'AWAKEN_BUILD_SOURCE_DIGEST': digest}, root=ROOT, verify=verify,
                                    record=record, run=run)
    try:
        unchanged = source_snapshot()[0] == digest
        context_unchanged = True
        try:
            build_context(ROOT, arguments, context)
        except ValueError:
            context_unchanged = False
        status = status or int(not unchanged or not context_unchanged)
        document.update(status="built" if status == 0 else "failed", rows=rows,
                        source_unchanged=unchanged, context_unchanged=context_unchanged, exit_code=status,
                        build_log_sha256=hashlib.sha256(build_log.read_bytes()).hexdigest()
                        if build_log.exists() else None)
        _write_receipt(receipt, document)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print("Cannot retain required build evidence: " + str(error), file=sys.stderr)
        status = status or 1
    return status


def verify_preparation(path: Path, arguments: list[str], digest: str) -> dict:
    data = path.read_bytes()
    document = json.loads(data)
    command = required_build_command(arguments)
    if not isinstance(document, dict):
        raise ValueError("Required build record must be an object")
    rows = document.get("rows", [])
    if (document.get("kind") != "required-libtest-build" or document.get("status") != "built"
            or document.get("exit_code") != 0 or document.get("source_unchanged") is not True
            or document.get("context_unchanged") is not True
            or document.get("source_digest") != digest or document.get("arguments") != arguments
            or document.get("command") != command or not isinstance(rows, list) or len(rows) != 1
            or not isinstance(rows[0], dict)
            or rows[0].get("command") != command or rows[0].get("exit_code") != 0
            or "subject_error" in rows[0] or "record_error" in rows[0]):
        raise ValueError("Required build does not bind this source and authored test graph")
    from _required_build_context import build_context
    build_context(ROOT, arguments, document.get("compile_context", {}))
    log = Path(document["build_log"])
    if log.resolve(strict=True).parent != path.resolve(strict=True).parent:
        raise ValueError("Required build log must share its evidence directory")
    if hashlib.sha256(log.read_bytes()).hexdigest() != document.get("build_log_sha256"):
        raise ValueError("Required build log changed")
    return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest()}


def run_required(arguments: list[str], inventory: Path, results: Path, receipt: Path,
                 prepared: Path | None = None) -> int:
    """Adapt two focused commands to the existing sequence, never run a loop."""
    from crate_test import execute_commands, native_command
    from rust_test_plan import source_snapshot, working_subject

    try:
        commands, required = required_commands(arguments)
    except ValueError as error:
        raise argparse.ArgumentError(None, str(error)) from error
    if (inventory.parent != results.parent or receipt.parent != results.parent or receipt.exists()
            or (prepared is not None and prepared.parent != receipt.parent)):
        raise ValueError("required execution needs one fresh evidence directory")
    inventory.touch()
    results.touch()
    digest, facts = working_subject()
    index = facts["index_tree"].decode().strip()
    if source_snapshot()[0] != digest:
        raise ValueError("Source changed while binding required execution facts")
    preparation = verify_preparation(prepared, arguments, digest) if prepared is not None else None
    runtime = execution_inputs()
    (receipt.parent / "working-tree.patch").write_bytes(facts["patch"])
    document = {"schema_version": 1, "kind": "required-libtest", "full_gate": False,
                "source_kind": "working-tree", "revision": facts["revision"].decode().strip(),
                "source_digest": digest, "index_tree": index,
                "arguments": arguments, "required_cases": required, "commands": commands,
                "list_command": commands[0], "test_command": commands[1],
                "selected_cases": [], "executed_cases": [], "execution_inputs": runtime,
                "postgres_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_POSTGRES") == "1",
                "postgres_fixture_configured": "AWAKEN_TEST_DATABASE_URL" in os.environ,
                "s3_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_S3") == "1",
                "s3_fixture_configured": all(os.environ.get(name) for name in
                    ("AWAKEN_TEST_S3_ENDPOINT", "AWAKEN_TEST_S3_ACCESS_KEY", "AWAKEN_TEST_S3_SECRET_KEY")),
                "status": "running", "rows": [], "errors": []}
    if preparation is not None:
        document["preparation"] = preparation
    _write_receipt(receipt, document)
    def verify():
        if execution_inputs() != runtime:
            raise ValueError("Execution fixture or backend changed during required execution")
        if source_snapshot()[0] != digest:
            raise ValueError("Source changed before or during required execution")
        if preparation is not None and verify_preparation(prepared, arguments, digest) != preparation:
            raise ValueError("Required build evidence changed before or during execution")
    def run(command, **kwargs):
        log = inventory if command == commands[0] else results
        with log.open("w", encoding="utf-8") as output:
            return subprocess.run(native_command(command, kwargs.get("env")), stdout=output, stderr=subprocess.STDOUT, **kwargs)
    def record(rows):
        document["rows"] = rows
        row = rows[-1]
        document["command_exit_code"] = row["exit_code"]
        document["duration_seconds"] = sum(item["duration_seconds"] for item in rows)
        errors = []
        if row["exit_code"] == 0:
            if len(rows) == 1:
                selected, errors = _listed_selection(inventory.read_text(encoding="utf-8"), required)
                document["selected_cases"] = sorted(selected.elements())
            else:
                observed = execution_evidence(inventory.read_text(encoding="utf-8"),
                                              results.read_text(encoding="utf-8"), row["exit_code"], required)
                document.update(observed)
                errors = observed["errors"]
        document["errors"] = errors
        _write_receipt(receipt, document)
        if errors:
            raise ValueError("; ".join(errors))
    return execute_commands(commands, {**os.environ, 'AWAKEN_BUILD_SOURCE_DIGEST': digest}, root=ROOT, verify=verify, record=record, run=run)[0]


def validate_receipt(inventory: Path, results: Path, status: int, receipt: Path,
                     required: list[str] | tuple[str, ...] = ()) -> dict:
    """Finalize an observed outer terminal; a dead parent leaves partial data."""
    from rust_test_plan import source_snapshot

    document, errors = {"schema_version": 1, "kind": "required-libtest", "source_kind": "unknown"}, []
    try:
        captured = json.loads(receipt.read_text(encoding="utf-8"))
        if not isinstance(captured, dict) or captured.get("kind") != "required-libtest":
            raise ValueError("required execution record has an unknown kind")
        document = captured
        document["required_cases"] = list(dict.fromkeys([*document["required_cases"], *required]))
        observed = execution_evidence(inventory.read_text(encoding="utf-8"), results.read_text(encoding="utf-8"),
                                      status, document["required_cases"])
        document.update(observed)
        errors = observed["errors"]
        if "preparation" in document:
            preparation = document["preparation"]
            prepared = Path(preparation["path"])
            if prepared.parent != receipt.parent or verify_preparation(prepared, document["arguments"], document["source_digest"]) != preparation:
                errors.append("Required build evidence changed")
        if source_snapshot()[0] != document["source_digest"]:
            errors.append("Source changed during required execution")
            document["source_unchanged"] = False
        else:
            document["source_unchanged"] = True
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        errors.append("Cannot qualify required execution evidence: " + str(error))
        document["source_unchanged"] = False
    # Original child/outer failure always outranks these validation faults.
    # Retain the actual outer terminal separately from each original Cargo row
    # and from this final validator's effective result. No row is rewritten.
    document.update(status="failed" if status or errors else "passed", command_exit_code=status,
                    exit_code=status or int(bool(errors)),
                    errors=errors, full_gate=False)
    _write_receipt(receipt, document)
    return document


class ExecutionTests(unittest.TestCase):
    def rustup_cargo_proxy(self, rustup: str | None = None) -> Path:
        if rustup is None:
            rustup = shutil.which("rustup")
        self.assertIsNotNone(rustup, "the gate installs rustup")
        # PATH can expose a symlink beside an unrelated Cargo admission
        # wrapper. The proxy belongs beside rustup's resolved executable.
        proxy = Path(rustup).resolve().with_name("cargo.exe" if os.name == "nt" else "cargo")
        self.assertTrue(proxy.is_file(), proxy)
        return proxy

    @unittest.skipIf(os.name == "nt", "POSIX symlinked rustup discovery")
    def test_rustup_proxy_discovery_ignores_an_adjacent_admission_wrapper(self) -> None:
        # PROXY-PATH causes: direct or symlinked rustup on PATH; the symlink's
        # directory also has a different Cargo. Both cases select the proxy
        # beside the real rustup, without invoking the host build queue.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real, exposed = root/"real", root/"exposed"
            real.mkdir(); exposed.mkdir()
            (real/"rustup").write_text("rustup")
            (real/"cargo").write_text("rustup proxy")
            (exposed/"rustup").symlink_to(real/"rustup")
            (exposed/"cargo").write_text("unrelated admission wrapper")
            for selected in (real/"rustup", exposed/"rustup"):
                with self.subTest(selected=selected), mock.patch.object(shutil, "which", return_value=str(selected)):
                    self.assertEqual(self.rustup_cargo_proxy(), real/"cargo")

    def test_execution_subject_is_runtime_input_and_contains_no_fixture_secrets(self):
        with mock.patch.dict(os.environ,{},clear=True):
            self.assertEqual(execution_inputs(),{"fixture":None,"crash_backend":None})
            os.environ.update(AWAKEN_TEST_FIXTURE_KIND="postgres",AWAKEN_TEST_FIXTURE_OWNER="owned-pg",
                              AWAKEN_TEST_DATABASE_URL="postgresql://secret-user:secret-password@localhost/db",
                              AWAKEN_CRASH_BACKEND="postgres")
            subject=execution_inputs()
            self.assertEqual(subject,{"fixture":{"kind":"postgres","owner":"owned-pg"},"crash_backend":"postgres"})
            self.assertNotIn("secret",json.dumps(subject))
            os.environ["AWAKEN_TEST_FIXTURE_OWNER"]="another-pg"
            self.assertNotEqual(execution_inputs(),subject)
            os.environ["AWAKEN_CRASH_BACKEND"]="sqlite"
            self.assertNotEqual(execution_inputs(),subject)

    def test_execution_subject_drift_stops_the_real_command_sequence(self):
        # The actual sequence must fence the boundary after inventory and before
        # execution. Only Cargo transport/source acquisition are controlled.
        for variable, replacement in (("AWAKEN_TEST_FIXTURE_OWNER", "another-owner"),
                                      ("AWAKEN_CRASH_BACKEND", "sqlite"), (None, None)):
            with self.subTest(variable=variable), tempfile.TemporaryDirectory() as temporary:
                directory=Path(temporary)
                facts={"index_tree":b"index", "revision":b"revision", "patch":b""}
                calls=[]
                def cargo(command, **kwargs):
                    calls.append(command)
                    if "--list" in command:
                        kwargs["stdout"].write("critical: test\n")
                        if variable: os.environ[variable]=replacement
                    else:
                        kwargs["stdout"].write("test critical ... ok\n"
                            "test result: ok. 1 passed; 0 failed; 0 ignored;\n")
                    return subprocess.CompletedProcess(command,0)
                environment={"AWAKEN_TEST_FIXTURE_KIND":"postgres",
                    "AWAKEN_TEST_FIXTURE_OWNER":"owned-fixture", "AWAKEN_CRASH_BACKEND":"postgres"}
                with mock.patch.dict(os.environ,environment,clear=True), \
                     mock.patch("rust_test_plan.working_subject",return_value=("digest",facts)), \
                     mock.patch("rust_test_plan.source_snapshot",return_value=("digest",{})), \
                     mock.patch("crate_test.native_command",side_effect=lambda command,env:command), \
                     mock.patch.object(subprocess,"run",side_effect=cargo), \
                     contextlib.redirect_stdout(io.StringIO()):
                    status=run_required(["-p","critical","--lib"],directory/"list.log",
                                        directory/"results.log",directory/"results.json")
                receipt=json.loads((directory/"results.json").read_text(encoding="utf-8"))
                self.assertEqual(status,1 if variable else 0)
                self.assertEqual(len(calls),1 if variable else 2)
                if variable:
                    self.assertIn("Execution fixture or backend changed",receipt["rows"][0]["subject_error"])
                else:
                    self.assertEqual(receipt["selected_cases"],["critical"])
                    self.assertEqual(receipt["executed_cases"],[["critical","ok"]])

    def test_execution_subject_refuses_partial_unknown_or_malformed_identity(self):
        for environment in ({"AWAKEN_TEST_FIXTURE_KIND":"postgres"},
                            {"AWAKEN_TEST_FIXTURE_OWNER":"owned-pg"},
                            {"AWAKEN_TEST_FIXTURE_KIND":"unknown","AWAKEN_TEST_FIXTURE_OWNER":"owned"},
                            {"AWAKEN_TEST_FIXTURE_KIND":"postgres","AWAKEN_TEST_FIXTURE_OWNER":"secret\nline"},
                            {"AWAKEN_CRASH_BACKEND":"unknown"}):
            with self.subTest(environment=environment),mock.patch.dict(os.environ,environment,clear=True):
                with self.assertRaises(ValueError):execution_inputs()

    def test_selection_projection_is_pure_but_execution_still_owns_capture(self):
        arguments=["-p","package","--require-case=case","--","--test-threads=1"]
        with mock.patch.dict(os.environ,{"RUST_TEST_NOCAPTURE":"1"}):
            self.assertEqual(selection_commands(arguments)[1],["case"])
            with self.assertRaisesRegex(ValueError,"owns captured output"):
                required_commands(arguments)

    def setUp(self):
        from _build_observation_fixture import synthetic_build_observations
        synthetic_build_observations(self)

    def test_execution_subject_is_runtime_input_and_contains_no_fixture_secrets(self):
        with mock.patch.dict(os.environ,{},clear=True):
            self.assertEqual(execution_inputs(),{"fixture":None,"crash_backend":None})
            os.environ.update(AWAKEN_TEST_FIXTURE_KIND="postgres",AWAKEN_TEST_FIXTURE_OWNER="owned-pg",
                              AWAKEN_TEST_DATABASE_URL="postgresql://secret-user:secret-password@localhost/db",
                              AWAKEN_CRASH_BACKEND="postgres")
            subject=execution_inputs()
            self.assertEqual(subject,{"fixture":{"kind":"postgres","owner":"owned-pg"},"crash_backend":"postgres"})
            self.assertNotIn("secret",json.dumps(subject))
            os.environ["AWAKEN_TEST_FIXTURE_OWNER"]="another-pg"
            self.assertNotEqual(execution_inputs(),subject)
            os.environ["AWAKEN_CRASH_BACKEND"]="sqlite"
            self.assertNotEqual(execution_inputs(),subject)

    def test_execution_subject_drift_stops_the_real_command_sequence(self):
        # The actual sequence must fence the boundary after inventory and before
        # execution. Only Cargo transport/source acquisition are controlled.
        for variable, replacement in (("AWAKEN_TEST_FIXTURE_OWNER", "another-owner"),
                                      ("AWAKEN_CRASH_BACKEND", "sqlite"), (None, None)):
            with self.subTest(variable=variable), tempfile.TemporaryDirectory() as temporary:
                directory=Path(temporary)
                facts={"index_tree":b"index", "revision":b"revision", "patch":b""}
                calls=[]
                def cargo(command, **kwargs):
                    calls.append(command)
                    if "--list" in command:
                        kwargs["stdout"].write("critical: test\n")
                        if variable: os.environ[variable]=replacement
                    else:
                        kwargs["stdout"].write("test critical ... ok\n"
                            "test result: ok. 1 passed; 0 failed; 0 ignored;\n")
                    return subprocess.CompletedProcess(command,0)
                environment={"AWAKEN_TEST_FIXTURE_KIND":"postgres",
                    "AWAKEN_TEST_FIXTURE_OWNER":"owned-fixture", "AWAKEN_CRASH_BACKEND":"postgres"}
                with mock.patch.dict(os.environ,environment,clear=True), \
                     mock.patch("rust_test_plan.working_subject",return_value=("digest",facts)), \
                     mock.patch("rust_test_plan.source_snapshot",return_value=("digest",{})), \
                     mock.patch("crate_test.native_command",side_effect=lambda command,env:command), \
                     mock.patch.object(subprocess,"run",side_effect=cargo), \
                     contextlib.redirect_stdout(io.StringIO()):
                    status=run_required(["-p","critical","--lib"],directory/"list.log",
                                        directory/"results.log",directory/"results.json")
                receipt=json.loads((directory/"results.json").read_text(encoding="utf-8"))
                self.assertEqual(status,1 if variable else 0)
                self.assertEqual(len(calls),1 if variable else 2)
                if variable:
                    self.assertIn("Execution fixture or backend changed",receipt["rows"][0]["subject_error"])
                else:
                    self.assertEqual(receipt["selected_cases"],["critical"])
                    self.assertEqual(receipt["executed_cases"],[["critical","ok"]])

    def test_execution_subject_refuses_partial_unknown_or_malformed_identity(self):
        for environment in ({"AWAKEN_TEST_FIXTURE_KIND":"postgres"},
                            {"AWAKEN_TEST_FIXTURE_OWNER":"owned-pg"},
                            {"AWAKEN_TEST_FIXTURE_KIND":"unknown","AWAKEN_TEST_FIXTURE_OWNER":"owned"},
                            {"AWAKEN_TEST_FIXTURE_KIND":"postgres","AWAKEN_TEST_FIXTURE_OWNER":"secret\nline"},
                            {"AWAKEN_CRASH_BACKEND":"unknown"}):
            with self.subTest(environment=environment),mock.patch.dict(os.environ,environment,clear=True):
                with self.assertRaises(ValueError):execution_inputs()

    def test_selection_projection_is_pure_but_execution_still_owns_capture(self):
        arguments=["-p","package","--require-case=case","--","--test-threads=1"]
        with mock.patch.dict(os.environ,{"RUST_TEST_NOCAPTURE":"1"}):
            self.assertEqual(selection_commands(arguments)[1],["case"])
            with self.assertRaisesRegex(ValueError,"owns captured output"):
                required_commands(arguments)

    @staticmethod
    def runner_fixture(fixture: Path, *entrypoint_inputs: str) -> Path:
        from formal_plan import script_closure
        from formal_sources import detached_git_environment
        root = Path(__file__).resolve().parents[2]
        inputs = script_closure(root, ("scripts/ci/_native_shell.py", "scripts/ci/_rust_test_execution.sh",
                                      "scripts/ci/check_rust_test_execution.py", *entrypoint_inputs))
        for relative in sorted(inputs):
            target = fixture / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / relative, target)
        # Match the repository's generated-output boundary. A lazily imported
        # build-context module creates bytecode after the first source capture;
        # that cache is execution output, while every source edit stays bound.
        (fixture / ".gitignore").write_text("target/\n__pycache__/\n*.py[cod]\n", encoding="utf-8")
        (fixture / "subject.txt").write_bytes(b"original source\n")
        values = detached_git_environment()
        for arguments in (("init", "--quiet"), ("config", "core.autocrlf", "false"), ("add", "."),
                          ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
            subprocess.run(["git", "-C", str(fixture), *arguments], env=values, check=True)
        return fixture / "scripts/ci"

    def test_fixture_bytecode_is_output_but_source_edits_remain_bound(self) -> None:
        # DT-FIXTURE: F1 unchanged source + a later Python import cache -> same
        # digest; F2 a later source edit -> different digest. The existing real
        # preparation/deadline cases import the module during actual execution.
        # Never exempt a script, change the production snapshot, or disable its
        # drift refusal merely to make the fixture execute.
        import rust_test_plan
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            scripts = self.runner_fixture(fixture)
            with mock.patch.object(rust_test_plan, "ROOT", fixture):
                before = rust_test_plan.source_snapshot()[0]
                cache = scripts / "__pycache__"
                cache.mkdir(exist_ok=True)
                (cache / "late.cpython.pyc").write_bytes(b"generated cache")
                self.assertEqual(rust_test_plan.source_snapshot()[0], before, "F1")
                (fixture / "subject.txt").write_text("changed source\n", encoding="utf-8")
                self.assertNotEqual(rust_test_plan.source_snapshot()[0], before, "F2")

    @staticmethod
    def required_shell(body: str, *, cwd: Path, env: dict[str, str], timeout: int):
        from _native_shell import bash_executable
        if os.name != "nt":
            return subprocess.run([bash_executable(environment=env), "-c", body], cwd=cwd, env=env,
                                  capture_output=True, text=True, timeout=timeout)
        # The script is ignored fixture evidence. The existing native transport
        # owns a kill-on-close Job before launching Git Bash, so an outer timeout
        # cannot strand fixture Cargo children or resolve System32's WSL Bash.
        with tempfile.NamedTemporaryFile(mode="w", suffix=".sh", dir=cwd / "target",
                                         encoding="utf-8", newline="\n", delete=False) as script:
            script.write(body + "\n")
        return subprocess.run([sys.executable, str(cwd / "scripts/ci/_native_shell.py"), script.name],
                              cwd=cwd, env=env, capture_output=True, text=True,
                              timeout=max(timeout, 120))

    @staticmethod
    def cargo_fixture(fixture: Path, body: str) -> dict[str, str]:
        from formal_sources import detached_git_environment
        (fixture / "target").mkdir(exist_ok=True)
        binary = fixture / "bin"
        binary.mkdir(exist_ok=True)
        cargo = binary / "cargo"
        cargo.write_text('''#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then
  sleep "${QUEUE_DELAY:-0}"
  [ "${ADMISSION_STATUS:-0}" = 0 ] || exit "$ADMISSION_STATUS"
  [ -z "${DEADLINE_COORDINATE:-}" ] || printf '%s\\n' "$$" > "$DEADLINE_COORDINATE"
  # The host's real rustup proxy, when named, enters the job exactly as an
  # admitted Cargo does: it injects its toolchain/home selection first.
  [ -z "${RUSTUP_PROXY_CARGO:-}" ] || exec "$RUSTUP_PROXY_CARGO" "$@"
  entry=$(command -v "cargo-$1"); exec "$entry" "$@"
fi
case " $* " in *" --no-run "*)
  printf '%s\\n' "$*" >> "$CARGO_TARGET_DIR/preparation-calls"
  sleep "${PREPARATION_DELAY:-0}"
  [ "${MUTATE_PREPARATION:-0}" != 1 ] || printf 'changed source\\n' > subject.txt
  exit "${PREPARATION_STATUS:-0}"
  ;;
esac
''' + body, encoding="utf-8")
        cargo.chmod(0o755)
        values = dict(detached_git_environment(), PATH=str(binary) + os.pathsep + os.environ['PATH'],
                      CARGO_TARGET_DIR=str(fixture / "target"))
        if os.name == "nt":
            from _native_test_launcher import native_fixture_launcher
            from _native_shell import bash_executable
            shutil.copyfile(native_fixture_launcher(), binary / "cargo.exe")
            values.update(AWAKEN_FIXTURE_PROGRAM=bash_executable(), AWAKEN_FIXTURE_SCRIPT=str(cargo))
        return values

    def test_subprocess_bytecode_is_not_source_drift_but_source_edits_are(self) -> None:
        # A fresh child imports the copied runner after its source snapshot.
        # Interpreter caches must not change that snapshot; authored bytes must.
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            self.runner_fixture(fixture)
            values = self.cargo_fixture(fixture, "exit 99\n")
            values.pop("PYTHONDONTWRITEBYTECODE", None)
            values.pop("PYTHONPYCACHEPREFIX", None)
            for mutate, expected in ((False, 0), (True, 1)):
                with self.subTest(mutate=mutate):
                    values["MUTATE_PREPARATION"] = str(int(mutate))
                    directory = fixture / "target" / ("edited" if mutate else "original")
                    directory.mkdir()
                    receipt = directory / "build.json"
                    result = subprocess.run(
                        [sys.executable, str(fixture / "scripts/ci/check_rust_test_execution.py"),
                         "--prepare", "--build-log", str(directory / "build.log"),
                         "--receipt", str(receipt), "--", "-p", "critical", "--lib"],
                        cwd=fixture, env=values, capture_output=True, timeout=90,
                    )
                    self.assertEqual(result.returncode, expected, (result.stdout, result.stderr))
                    self.assertEqual(json.loads(receipt.read_text())["status"],
                                     "failed" if mutate else "built")
                    self.assertTrue(list((fixture / "scripts/ci/__pycache__").glob("*.pyc")))

    def test_prepared_graph_refuses_compile_environment_config_and_tool_drift(self) -> None:
        import rust_test_plan
        from _required_build_context import build_context
        for fault in ("CARGO_BUILD_TARGET", "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "LIB", "INCLUDE", "LINK", "config", "tool"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)/"repo"; fixture.mkdir(); self.runner_fixture(fixture)
                values = self.cargo_fixture(fixture, "exit 99\n")
                cargo_home = Path(temporary)/"cargo-home"; cargo_home.mkdir()
                values["CARGO_HOME"] = str(cargo_home)
                # This owned fake executable is an external compilation input,
                # excluded from the working-source digest, like installed tools.
                with (fixture/".gitignore").open("a") as stream: stream.write("bin/\n")
                directory = fixture/"target/evidence"; directory.mkdir()
                build = directory/"build.json"; arguments = ["-p", "critical", "--lib"]
                with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                     mock.patch.object(rust_test_plan, "ROOT", fixture):
                    self.assertEqual(prepare_required(arguments, directory/"build.log", build), 0)
                    original_source = rust_test_plan.source_snapshot()[0]
                    if fault == "config": (cargo_home/"config.toml").write_text("[build]\ntarget='different'\n")
                    elif fault == "tool":
                        tool = fixture/"bin"/("cargo.exe" if os.name == "nt" else "cargo")
                        with tool.open("ab") as stream: stream.write(b"\nchanged-tool-identity\n")
                    else: os.environ[fault] = "different-compilation-input"
                    self.assertEqual(rust_test_plan.source_snapshot()[0], original_source)
                    with self.assertRaisesRegex(ValueError, "changed"):
                        verify_preparation(build, arguments, original_source)
                    self.assertFalse((directory/"results.json").exists())

    def test_configuration_is_cargo_owned_and_external_files_are_bound(self) -> None:
        from _required_build_context import build_context
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = root/"external-inputs"
            config.write_text('[build]\nrustc="opaque/compiler"\n[alias]\nlinker="check"\n')
            options = (["--config", 'build.rustflags=["-C", "linker=rust-lld"]'],
                       ['--config=build.rustflags=["-C", "linker=rust-lld"]'],
                       ["--config", str(config)], ["--config="+str(config)])
            for option in options:
                with self.subTest(option=option), \
                     mock.patch.dict(os.environ, {"CARGO_HOME": str(root/"cargo-home"), "RUSTFLAGS": "-C linker=rust-lld"}), \
                     mock.patch("subprocess.check_output") as query:
                    context = build_context(root, ["-p", "critical", "--lib", *option])
                    self.assertEqual(context["kind"], "known-compilation-inputs")
                    if str(config) in " ".join(option):
                        self.assertIn({"path": str(config.resolve()), "sha256": hashlib.sha256(config.read_bytes()).hexdigest()}, context["files"])
                    query.assert_not_called()

    def test_changed_rustup_is_rejected_before_it_can_execute(self) -> None:
        import rust_test_plan
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary); self.runner_fixture(fixture)
            values = self.cargo_fixture(fixture, "exit 99\n")
            name = "rustup.exe" if os.name == "nt" else "rustup"
            target = fixture/"bin"/name
            original = shutil.which("rustup")
            self.assertIsNotNone(original); shutil.copy2(original, target); target.chmod(0o755)
            with (fixture/".gitignore").open("a") as stream: stream.write("bin/\n")
            directory = fixture/"target/evidence"; directory.mkdir()
            arguments = ["-p", "critical", "--lib"]
            with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                 mock.patch.object(rust_test_plan, "ROOT", fixture):
                self.assertEqual(prepare_required(arguments, directory/"build.log", directory/"build.json"), 0)
                digest = rust_test_plan.source_snapshot()[0]
                replacement = target.with_name("replacement-rustup")
                replacement.write_bytes(b"a replaced resolver must never execute")
                replacement.replace(target)
                self.assertEqual(rust_test_plan.source_snapshot()[0], digest)
                with self.assertRaisesRegex(ValueError, "tool identity changed"):
                    verify_preparation(directory/"build.json", arguments, digest)


    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
    def test_proxy_selected_toolchain_judges_the_same_context_outside_cargo(self) -> None:
        # DT-PROXY row P1, end to end: preparation and execution run inside the
        # host's real rustup Cargo proxy, which injects CARGO_HOME, RUSTUP_HOME
        # and RUSTUP_TOOLCHAIN; the final validator runs outside it. All tests
        # pass, so the shell must pass: the unchanged selection is one context.
        rustup = shutil.which("rustup")
        self.assertIsNotNone(rustup, "the Linux gate installs rustup")
        proxy = self.rustup_cargo_proxy(rustup)
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary); self.runner_fixture(fixture)
            environment = self.cargo_fixture(fixture, '''
case "$*" in
  *--list*) echo 'critical: test'; exit 0 ;;
esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
            for name in PROXY_INJECTED:
                environment.pop(name, None)
            environment["RUSTUP_PROXY_CARGO"] = str(proxy)
            result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; "
                                     "run_required_rust_tests -p critical --lib critical -- --exact"],
                                    cwd=fixture, env=environment, capture_output=True, text=True, timeout=120)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            prepared = json.loads(next((fixture/"target/test-timings").glob("rust-execution.*/build.json")).read_text())
            self.assertTrue({"CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"} <= set(prepared["compile_context"]["environment"]))

    @unittest.skipIf(os.name == "nt", "the POSIX proxy probe runs in the Linux gate")
    def test_proxy_injected_selection_decision_table(self) -> None:
        # DT-PROXY (_required_build_context): the build context is recorded by
        # a probe that the host's real rustup Cargo proxy starts, and judged in
        # this process, outside any proxy, as the final validator is.
        # P1 caller leaves CARGO_HOME/RUSTUP_HOME/RUSTUP_TOOLCHAIN unset; proxy
        #    injects homes and the file-selected toolchain             -> same
        # P2 caller names the default homes explicitly at check         -> same
        # P3 caller CARGO_HOME (P3a) / RUSTUP_HOME (P3b) differs at check -> changed
        # P4 caller short override "<channel>" at build, unset at check -> same
        # P5 caller selects another toolchain (a path) at check         -> changed
        # P6 caller selection rustup cannot resolve at check            -> changed
        # P7 no rustup on PATH: no proxy, literal values compared       -> same/changed
        # P8 rustup is a symlink beside an unrelated Cargo wrapper     -> same
        #    P8's shadow exits 97 if selected; only the resolved rustup proxy
        #    may execute this no-build context probe. Build admission is unchanged.
        from _required_build_context import build_context
        rustup = shutil.which("rustup")
        self.assertIsNotNone(rustup, "the gate installs rustup")
        arguments = ["-p", "critical", "--lib"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)/"repo"; root.mkdir()
            shutil.copy2(ROOT/"rust-toolchain.toml", root/"rust-toolchain.toml")
            probe_directory = Path(temporary)/"probe"; probe_directory.mkdir()
            probe = probe_directory/"awaken-context-probe.py"
            probe.write_text(f"""import json, os, sys
from pathlib import Path
sys.path.insert(0, {str(Path(__file__).resolve().parent)!r})
from _required_build_context import build_context
print(json.dumps({{"context": build_context(Path.cwd(), {arguments!r}),
                  "injected": {{name: os.environ.get(name) for name in {PROXY_INJECTED!r}}}}}))
""", encoding="utf-8")
            launcher = probe_directory/"cargo-awaken-context-probe"
            launcher.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{probe}" "$@"\n', encoding="utf-8")
            launcher.chmod(0o755)
            caller = {name: value for name, value in os.environ.items() if name.upper() not in PROXY_INJECTED}
            caller["PATH"] = str(probe_directory) + os.pathsep + caller.get("PATH", "")
            def proxied(*, rustup_path=rustup, **overrides):
                proxy = self.rustup_cargo_proxy(rustup_path)
                completed = subprocess.run([str(proxy), "awaken-context-probe"], cwd=root, capture_output=True,
                                           text=True, env={**caller, **overrides}, timeout=120)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                return json.loads(completed.stdout)
            def judged(recorded, **overrides):
                with mock.patch.dict(os.environ, {**caller, **overrides}, clear=True):
                    try:
                        build_context(root, arguments, recorded)
                    except ValueError as error:
                        return str(error)
                    return "same"
            build = proxied()
            alias_directory = Path(temporary)/"aliases"; alias_directory.mkdir()
            alias = alias_directory/"rustup"; alias.symlink_to(Path(rustup).resolve())
            shadow = alias_directory/"cargo"
            shadow.write_text('#!/bin/sh\nexit 97\n', encoding="utf-8"); shadow.chmod(0o755)
            self.assertEqual(proxied(rustup_path=str(alias)), build, "P8")
            injected = build["injected"]
            for name in ("CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"):
                self.assertTrue(injected[name], f"the proxy must inject {name}: {injected}")
            changed = "Required build compilation environment/configuration changed"
            self.assertEqual(judged(build["context"]), "same", "P1")
            self.assertEqual(judged(build["context"], CARGO_HOME=injected["CARGO_HOME"],
                                    RUSTUP_HOME=injected["RUSTUP_HOME"]), "same", "P2")
            other = str(Path(temporary)/"other-home")
            self.assertEqual(judged(build["context"], CARGO_HOME=other), changed, "P3a")
            self.assertEqual(judged(build["context"], RUSTUP_HOME=other), changed, "P3b")
            channel = (root/"rust-toolchain.toml").read_text(encoding="utf-8").split('channel = "')[1].split('"')[0]
            short = proxied(RUSTUP_TOOLCHAIN=channel)
            self.assertEqual(short["injected"]["RUSTUP_TOOLCHAIN"], injected["RUSTUP_TOOLCHAIN"])
            self.assertEqual(judged(short["context"]), "same", "P4")
            selected = str(Path(injected["RUSTUP_HOME"])/"toolchains"/injected["RUSTUP_TOOLCHAIN"])
            self.assertEqual(judged(build["context"], RUSTUP_TOOLCHAIN=selected), changed, "P5")
            self.assertEqual(judged(build["context"], RUSTUP_TOOLCHAIN="awaken-uninstalled-toolchain"), changed, "P6")
            without = {name: value for name, value in caller.items() if name.upper() != "PATH"}
            without["PATH"] = str(probe_directory)
            with mock.patch.dict(os.environ, without, clear=True):
                literal = build_context(root, arguments)
            with mock.patch.dict(os.environ, {**without, "RUSTUP_TOOLCHAIN": channel}, clear=True):
                self.assertRaisesRegex(ValueError, changed, build_context, root, arguments, literal)
            with mock.patch.dict(os.environ, without, clear=True):
                self.assertEqual(build_context(root, arguments, literal), literal, "P7")

    def test_prepare_initial_source_refusal_preserves_original_exit(self) -> None:
        import rust_test_plan
        for status in (75, 23, -15):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                argv = [__file__, "--prepare", "--build-log", str(root/"build.log"),
                        "--receipt", str(root/"build.json"), "--", "-p", "critical", "--lib"]
                with mock.patch.object(sys, "argv", argv), \
                     mock.patch.object(rust_test_plan, "working_subject", side_effect=subprocess.CalledProcessError(status, ["git", "subject"])), \
                     mock.patch("crate_test.execute_commands") as execute, contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(main(), status if status >= 0 else 128-status)
                execute.assert_not_called(); self.assertFalse((root/"build.json").exists())

    @unittest.skipUnless(os.name == "nt", "native Windows Job and Cargo boundary")
    def test_windows_cold_build_execution_budget_and_owned_tree_cleanup(self) -> None:
        # Windows process startup and receipt finalization need headroom under
        # parallel CI load. Keep build > execution so a shared budget fails;
        # timed-out rows still require 124 and both owned processes terminated.
        import ctypes
        from ctypes import wintypes
        from _native_shell import bash_executable
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
        kernel.TerminateProcess.argtypes = (wintypes.HANDLE, wintypes.UINT)
        kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
        execution_seconds = 20
        preparation_seconds = execution_seconds + 2
        build_seconds = preparation_seconds + 30
        outer_seconds = build_seconds + execution_seconds + 45
        for row, expected in (("fast", 0), ("build-timeout", 124), ("execution-timeout", 124)):
            with self.subTest(row=row), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary); self.runner_fixture(fixture)
                tree = fixture/"owned-tree.py"
                tree.write_text("import json,os,subprocess,sys,time\nfrom pathlib import Path\n"
                                "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'])\n"
                                "path=Path(sys.argv[1]);pending=path.with_suffix('.pending');pending.write_text(json.dumps([os.getpid(),child.pid]));pending.replace(path)\n"
                                "print('ready',flush=True)\ntime.sleep(120)\n")
                body = "case \"$*\" in *--list*) echo 'critical: test'; exit 0 ;; esac\n"
                if row == "execution-timeout":
                    body += "exec " + shlex.quote(Path(sys.executable).as_posix()) + " " + shlex.quote(tree.as_posix()) + ' "$CARGO_TARGET_DIR/ready"\n'
                else: body += "echo 'test critical ... ok'\necho 'test result: ok. 1 passed; 0 failed; 0 ignored;'\n"
                values = self.cargo_fixture(fixture, body)
                values.update(PREPARATION_DELAY=str(preparation_seconds), AWAKEN_REQUIRED_TEST_BUILD_TIMEOUT_SECONDS="3" if row == "build-timeout" else str(build_seconds),
                              AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS=str(execution_seconds), AWAKEN_NATIVE_BASH=bash_executable())
                entry = fixture/"invoke.sh"
                entry.write_text("source scripts/ci/_rust_test_execution.sh\nrun_required_rust_tests -p critical --lib critical --require-case=critical\n", newline="\n")
                process = subprocess.Popen([sys.executable, str(fixture/"scripts/ci/deadline.py"), str(outer_seconds), "--",
                                            bash_executable(), entry.as_posix()], cwd=fixture, env=values,
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                handles = []
                try:
                    if row == "execution-timeout":
                        ready = fixture/"target/ready"; until = time.monotonic()+build_seconds+execution_seconds
                        # awaken-allow(wait): retain actual child process handles before the execution deadline kills the owned Job.
                        while not ready.exists() and process.poll() is None and time.monotonic() < until: time.sleep(.01)
                        self.assertTrue(ready.exists(), "owned execution did not start")
                        for pid in json.loads(ready.read_text()):
                            handle = kernel.OpenProcess(0x00100001, False, pid)
                            self.assertTrue(handle, "cannot retain owned process handle"); handles.append(handle)
                    output = process.communicate(timeout=outer_seconds+5)
                    self.assertEqual(process.returncode, expected, output)
                    directory = next((fixture/"target/test-timings").glob("rust-execution.*"))
                    if row == "build-timeout":
                        self.assertFalse((directory/"results.json").exists())
                        self.assertNotEqual(json.loads((directory/"build.json").read_text())["status"], "built")
                    else:
                        self.assertEqual(json.loads((directory/"build.json").read_text())["status"], "built")
                        result = json.loads((directory/"results.json").read_text())
                        self.assertEqual(result["exit_code"], expected)
                        if row == "fast":
                            self.assertEqual(result["status"], "passed")
                            self.assertEqual(result["selected_cases"], ["critical"])
                            self.assertEqual(result["executed_cases"], [["critical", "ok"]])
                            self.assertEqual(result["errors"], [])
                    for handle in handles: self.assertEqual(kernel.WaitForSingleObject(handle, 5000), 0)
                finally:
                    if process.poll() is None: process.kill(); process.communicate(timeout=10)
                    for handle in handles:
                        if kernel.WaitForSingleObject(handle, 0) != 0: kernel.TerminateProcess(handle, 1)
                        kernel.CloseHandle(handle)

    def test_build_keeps_authored_features_without_spending_libtest_arguments(self) -> None:
        arguments = ["-p", "critical", "--lib", "--features", "sqlite,test-support", "critical",
                     "--require-case=critical", "--", "--ignored", "--test-threads=1"]
        expected = ["cargo", "test", "--locked", "-p", "critical", "--lib", "--features",
                    "sqlite,test-support", "critical", "--no-run"]
        self.assertEqual(required_build_command(arguments), expected)
        self.assertEqual(required_build_command(["--locked", "-p", "critical", "--lib"])
                         .count("--locked"), 1)

    def test_build_fact_never_qualifies_execution_or_another_subject(self) -> None:
        # Decision rows: an exact successful build may prepare execution; build
        # alone, different source/argv, or altered evidence cannot qualify it.
        import rust_test_plan
        for fault in (None, "source", "arguments", "record"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary); self.runner_fixture(fixture)
                values = self.cargo_fixture(fixture, '''
printf '%s\n' "$*" >> "$CARGO_TARGET_DIR/calls"
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
                directory = fixture / "target/evidence"; directory.mkdir()
                build = directory / "build.json"; arguments = ["-p", "critical", "--lib"]
                inventory, results, receipt = [directory / name for name in ("list.log", "results.log", "results.json")]
                with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                     mock.patch.object(rust_test_plan, "ROOT", fixture):
                    self.assertEqual(prepare_required(arguments, directory / "build.log", build), 0)
                    data = json.loads(build.read_text()); self.assertEqual(data["status"], "built")
                    self.assertFalse(data["full_gate"])
                    self.assertNotIn("executed_cases", data)
                    self.assertFalse((fixture / "target/calls").exists())
                    if fault == "source": (fixture / "subject.txt").write_text("different source\n")
                    if fault == "arguments": arguments = ["-p", "another", "--lib"]
                    if fault == "record":
                        data["rows"][0]["exit_code"] = 23
                        build.write_text(json.dumps(data))
                    if fault is not None:
                        with self.assertRaisesRegex(ValueError, "does not bind"):
                            run_required(arguments, inventory, results, receipt, build)
                        self.assertFalse((fixture / "target/calls").exists())
                    else:
                        fake = directory / "not-execution.json"; fake.write_bytes(build.read_bytes())
                        inventory.write_text("critical: test\n")
                        results.write_text("test critical ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")
                        rejected = validate_receipt(inventory, results, 0, fake)
                        self.assertEqual(rejected["status"], "failed")
                        self.assertEqual(run_required(arguments, inventory, results, receipt, build), 0)
                        qualified = validate_receipt(inventory, results, 0, receipt)
                        self.assertEqual(qualified["status"], "passed")
                        self.assertEqual(qualified["selected_cases"], ["critical"])
                        data["build_log_sha256"] = "0" * 64; build.write_text(json.dumps(data))
                        self.assertEqual(validate_receipt(inventory, results, 0, receipt)["status"], "failed")

    def test_build_failure_source_drift_and_writer_fault_keep_first_cause(self) -> None:
        import rust_test_plan
        original_writer = _write_receipt
        for raw in (0, 23):
            for fault in ("source", "writer"):
                with self.subTest(raw=raw, fault=fault), tempfile.TemporaryDirectory() as temporary:
                    fixture = Path(temporary); self.runner_fixture(fixture)
                    values = self.cargo_fixture(fixture, "exit 99\n")
                    values.update(PREPARATION_STATUS=str(raw), MUTATE_PREPARATION=str(int(fault == "source")))
                    directory = fixture / "target/evidence"; directory.mkdir()
                    def write(path, document):
                        if fault == "writer" and document.get("status") in ("built", "failed"):
                            raise OSError("controlled build writer fault")
                        original_writer(path, document)
                    with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                         mock.patch.object(rust_test_plan, "ROOT", fixture), \
                         mock.patch(__name__ + "._write_receipt", side_effect=write), contextlib.redirect_stderr(io.StringIO()):
                        status = prepare_required(["-p", "critical", "--lib"], directory / "build.log", directory / "build.json")
                    self.assertEqual(status, raw or 1)
                    data = json.loads((directory / "build.json").read_text())
                    self.assertNotEqual(data["status"], "built")
                    self.assertEqual(data["rows"][0]["exit_code"], raw)

    @unittest.skipIf(os.name == "nt", "POSIX real deadline fixture; Windows runs build/source/receipt rows")
    def test_cold_build_does_not_consume_or_extend_shared_execution_deadline(self) -> None:
        # Build3s > list/run2s: separate8s preparation succeeds. A real3s test
        # still times out124 inside that same2s list/run deadline, never4s.
        for slow, expected in ((False, 0), (True, 124)):
            with self.subTest(slow=slow), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary); self.runner_fixture(fixture)
                values = self.cargo_fixture(fixture, '''
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
sleep "$TEST_DELAY"
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
                values.update(PREPARATION_DELAY="3", TEST_DELAY="3" if slow else "0",
                              AWAKEN_REQUIRED_TEST_BUILD_TIMEOUT_SECONDS="8", AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS="2")
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical --require-case=critical"],
                                        cwd=fixture, env=values, capture_output=True, text=True, timeout=25)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                builds = list((fixture / "target/test-timings").glob("rust-execution.*/build.json"))
                self.assertEqual(len(builds), 1); self.assertEqual(json.loads(builds[0].read_text())["status"], "built")
                data = json.loads(builds[0].with_name("results.json").read_text())
                self.assertEqual(data["status"], "failed" if slow else "passed")
                self.assertEqual(data["exit_code"], expected)

    @unittest.skipIf(os.name == "nt", "POSIX admitted shell composition")
    def test_shared_budget_starts_after_queued_admission(self) -> None:
        # E9: queue2.3s > execution2s, while admitted list/run fit2s. The
        # actual shell must return child0/23, not charge waiting and return124.
        # Queue refusal75 starts no Cargo test or source-bound body. Only the
        # fixture Cargo protocol delays; the real deadline owns the clock.
        # Revision/index/patch share the initial canonical capture; later
        # fences stay fresh. Real Git and the2s/2.3s/15s values remain active.
        for status in (0, 23, 75):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                values = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CARGO_TARGET_DIR/calls"
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
exit "$RESULT_STATUS"
''')
                values.update(QUEUE_DELAY="2.3", AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS="2",
                              ADMISSION_STATUS="75" if status == 75 else "0", RESULT_STATUS=str(status))
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical"],
                                        cwd=fixture, env=values, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                calls = fixture / "target/calls"
                self.assertEqual(calls.exists(), status != 75)
                if status != 75:
                    self.assertEqual(len(calls.read_text().splitlines()), 2)

    @unittest.skipIf(os.name == "nt", "private POSIX process/source fixture")
    def test_source_and_record_faults_preserve_the_actual_child_status(self) -> None:
        # E5/E8/E12 decision table: child0/23/75/signal15 x tracked-source
        # mutation after run OR receipt writer fault after run. Zero becomes1;
        # original nonzero survives. Real private Git and child exit are used;
        # only the exact post-run source/writer boundary is controlled.
        import rust_test_plan
        original_writer = _write_receipt
        for raw in (0, 23, 75, 143):
            for cause in ("source", "writer"):
                with self.subTest(raw=raw, cause=cause), tempfile.TemporaryDirectory() as temporary:
                    fixture = Path(temporary)
                    self.runner_fixture(fixture)
                    values = self.cargo_fixture(fixture, '''
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
[ "$MUTATE_SOURCE" != 1 ] || printf 'changed source\\n' > subject.txt
[ "$RESULT_STATUS" != 143 ] || kill -TERM "$$"
exit "$RESULT_STATUS"
''')
                    values.update(RESULT_STATUS=str(raw), MUTATE_SOURCE=str(int(cause == "source")))
                    directory = fixture / "target/evidence"
                    directory.mkdir(parents=True)
                    faults = []
                    def writer(path, document):
                        if cause == "writer" and len(document["rows"]) == 2:
                            faults.append("post-run writer")
                            raise OSError("controlled post-run writer fault")
                        original_writer(path, document)
                    diagnostics = io.StringIO()
                    with mock.patch.dict(os.environ, values, clear=True), \
                         mock.patch(__name__ + ".ROOT", fixture), mock.patch.object(rust_test_plan, "ROOT", fixture), \
                         mock.patch(__name__ + "._write_receipt", side_effect=writer), contextlib.redirect_stderr(diagnostics):
                        status = run_required(["-p", "critical", "--lib"], directory / "list.log",
                                              directory / "results.log", directory / "results.json")
                    self.assertEqual(status, raw or 1)
                    self.assertIn("test critical ... ok", (directory / "results.log").read_text())
                    if cause == "source":
                        self.assertEqual((fixture / "subject.txt").read_text(), "changed source\n")
                        data = json.loads((directory / "results.json").read_text())
                        self.assertIn("Source changed", data["rows"][-1]["subject_error"])
                    else:
                        self.assertEqual(faults, ["post-run writer"])
                        self.assertIn("controlled post-run writer fault", diagnostics.getvalue())

    @unittest.skipIf(os.name == "nt", "actual private libtest terminal fixture")
    def test_terminal_source_and_writer_failures_preserve_outer_status(self) -> None:
        # E5/E12: an actual child0/23/75/signal15 terminates first. Only then
        # fail the exact terminal reader/writer; assert the fault was reached.
        # The offline validator returns1, while its caller's original nonzero
        # remains authoritative. A different exception is not this negative.
        import rust_test_plan
        for raw in (0, 23, 75, 143):
            for cause in ("reader", "writer"):
                with self.subTest(raw=raw, cause=cause), tempfile.TemporaryDirectory() as temporary:
                    fixture = Path(temporary)
                    self.runner_fixture(fixture)
                    values = self.cargo_fixture(fixture, '''
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
[ "$RESULT_STATUS" != 143 ] || kill -TERM "$$"
exit "$RESULT_STATUS"
''')
                    values['RESULT_STATUS'] = str(raw)
                    directory = fixture / "target/evidence"
                    directory.mkdir(parents=True)
                    paths = [directory / name for name in ("list.log", "results.log", "results.json")]
                    with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                         mock.patch.object(rust_test_plan, "ROOT", fixture):
                        status = run_required(["-p", "critical", "--lib"], *paths)
                        self.assertEqual(status, raw)
                        argv = [__file__, "--inventory", str(paths[0]), "--results", str(paths[1]),
                                "--exit-code", str(status), "--receipt", str(paths[2])]
                        point = "rust_test_plan.source_snapshot" if cause == "reader" else __name__ + "._write_receipt"
                        diagnostics = io.StringIO()
                        with mock.patch.object(sys, "argv", argv), mock.patch(point, side_effect=OSError("controlled terminal " + cause)) as fault, \
                             contextlib.redirect_stderr(diagnostics):
                            validation = main()
                        fault.assert_called_once()
                        self.assertIn("controlled terminal " + cause, diagnostics.getvalue())
                        self.assertEqual(validation, 1)
                        self.assertEqual(status or validation, raw or 1)

    def test_floor_and_literal_argv_share_one_libtest_selection(self) -> None:
        # E3/E11: metadata floor is consumed only before--. Existing locked,
        # exact/ignored/include-ignored/skip and literals (including newline,
        # quotes and shell syntax) remain identical in list/run. A list which
        # passes all its cases but omitted a mandatory identity still rejects.
        literal = "literal with spaces\n$(touch forbidden);`text`"
        args = ["-p", "critical", "--locked", "--require-case=critical", "--",
                "--exact", "--ignored", "--include-ignored", "--skip", "other", literal,
                "--require-case=literal-test-filter"]
        commands, floor = required_commands(args)
        self.assertEqual(floor, ["critical"])
        self.assertEqual(commands[0][:-1], commands[1][:-1])
        self.assertEqual(commands[0].count("--locked"), 1)
        self.assertEqual(commands[0][-2], "--require-case=literal-test-filter")
        self.assertIn(literal, commands[0])
        self.assertEqual(commands[0][-1], "--list")
        self.assertEqual(commands[1][-1], "--show-output")
        inventory = "other: test\n"
        output = "test other ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_evidence(inventory, output, 0)["errors"], [])
        self.assertIn("required selection omits cases: critical", execution_evidence(inventory, output, 0, floor)["errors"])

    def test_uncaptured_output_is_refused_before_build_and_cannot_mimic_named_results(self) -> None:
        # A real observation printed metrics between libtest's name and `ok`.
        # C1 an argv switch or ambient nonzero/empty NOCAPTURE -> reject before
        # either build or execution. C2 normal capture -> preserve all literal
        # selection and show buffered diagnostics. C3 partial/interleaved name
        # rows cannot qualify from the final passing count alone.
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("RUST_TEST_NOCAPTURE", None)
            for switch in ("--nocapture", "--no-capture", "--nocapture=true"):
                args=["-p", "critical", "--", switch]
                for planner in (required_commands, required_build_command):
                    with self.subTest(switch=switch, planner=planner.__name__):
                        with self.assertRaisesRegex(ValueError, "owns captured output"): planner(args)
            for value in ("", "1", "false"):
                os.environ["RUST_TEST_NOCAPTURE"]=value
                with self.subTest(value=value), self.assertRaisesRegex(ValueError, "RUST_TEST_NOCAPTURE"):
                    required_build_command(["-p", "critical"])
            os.environ["RUST_TEST_NOCAPTURE"]="0"
            commands,_=required_commands(["-p", "critical", "--", "--skip=--nocapture"])
            self.assertIn("--skip=--nocapture",commands[0]);self.assertEqual(commands[1][-1],"--show-output")
        inventory="critical: test\n"
        interleaved="test critical ... metric=10\nok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertTrue(execution_evidence(inventory,interleaved,0,["critical"])["errors"])
        # Direct tracing writers can interleave even with libtest capture on.
        # A successes list/summary cannot repair that missing terminal record.
        # The HTTP fixture must use a span-only subscriber; keep this strict.
        traced=("test critical ... 2026-10-26T22:18:05Z  INFO opentelemetry: "
                "name=\"MeterProvider.GlobalSet\" provider installed\n"
                "ok\nsuccesses:\n    critical\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored;\n")
        self.assertTrue(execution_evidence(inventory,traced,0,["critical"])["errors"])
        captured="test critical ... ok\nsuccesses:\nmetric=10\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_evidence(inventory,captured,0,["critical"])["errors"],[])

    def test_floor_failure_stops_before_run(self) -> None:
        # E3/E4: listing returns0 with a nonempty wrong identity, while a run
        # would report success. Mandatory floor refuses before any run process.
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            self.runner_fixture(fixture)
            values = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CARGO_TARGET_DIR/calls"
echo 'other: test'
''')
            result = self.required_shell("source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical --require-case=critical", cwd=fixture, env=values, timeout=15)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("required selection omits cases: critical", result.stderr)
            self.assertEqual(len((fixture / "target/calls").read_text().splitlines()), 1)

    def test_hidden_index_and_untracked_bytes_are_working_source_causes(self) -> None:
        # E7/E8: index and WT cancel in HEAD->WT, or an existing untracked
        # path changes bytes. Both are real source changes after listing, so
        # the old raw patch is insufficient and no run may start. Fixture Git
        # inherits the existing detached environment, never a parent hook.
        for cause in ("index", "untracked"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                (fixture / "untracked.txt").write_text("first bytes\n")
                values = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CARGO_TARGET_DIR/calls"
case "$*" in *--list*)
  echo 'critical: test'
  if [ "$MUTATION_CAUSE" = index ]; then
    printf 'staged source\\n' > subject.txt
    git add subject.txt
    printf 'original source\\n' > subject.txt
  else
    printf 'second bytes\\n' > untracked.txt
  fi
  exit 0 ;;
esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
                values['MUTATION_CAUSE'] = cause
                result = self.required_shell("source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical", cwd=fixture, env=values, timeout=15)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("Source changed", result.stderr)
                self.assertEqual(len((fixture / "target/calls").read_text().splitlines()), 1)
                if cause == "index":
                    import formal_sources
                    self.assertEqual(formal_sources.git_output(fixture, "diff", "--binary", "HEAD"), b"")
                data = json.loads(next((fixture / "target/test-timings").glob("rust-execution.*/results.json")).read_text())
                self.assertEqual(data["status"], "failed")
                self.assertFalse(data["source_unchanged"])

    def test_required_source_uses_same_hook_index_and_detaches_foreign_hook(self) -> None:
        # E7: an actual same-root alternate index is the selected subject;
        # foreign DIR/WT/index must not replace it. Both runs use the real
        # shell/working reader. Protect every decoy config/index/ref/WT byte.
        import formal_sources
        for same_hook in (False, True):
            with self.subTest(same_hook=same_hook), tempfile.TemporaryDirectory() as temporary:
                outer = Path(temporary)
                own, decoy = outer / "own", outer / "decoy"
                own.mkdir(); decoy.mkdir()
                self.runner_fixture(own)
                self.runner_fixture(decoy)
                before = formal_sources.source_facts(decoy)
                protected = {path.relative_to(decoy).as_posix(): path.read_bytes()
                             for path in decoy.rglob("*") if path.is_file()}
                alternate = own / ".git/partial-index"
                shutil.copy2(own / ".git/index", alternate)
                values = self.cargo_fixture(own, '''
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
                if same_hook:
                    values.update(GIT_DIR=str(own / ".git"), GIT_WORK_TREE=str(own), GIT_INDEX_FILE=str(alternate))
                    subprocess.run(["git", "-C", str(own), "update-index", "--force-remove", "subject.txt"], env=values, check=True)
                    expected_index = subprocess.check_output(["git", "-C", str(own), "write-tree"], env=values).decode().strip()
                    self.assertNotEqual(expected_index, formal_sources.git_output(own, "write-tree").decode().strip())
                else:
                    values.update(GIT_DIR=str(decoy / ".git"), GIT_WORK_TREE=str(decoy), GIT_INDEX_FILE=str(decoy / ".git/index"), GIT_PREFIX="foreign/")
                    expected_index = formal_sources.git_output(own, "write-tree").decode().strip()
                result = self.required_shell("source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical", cwd=own, env=values, timeout=15)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                data = json.loads(next((own / "target/test-timings").glob("rust-execution.*/results.json")).read_text())
                self.assertEqual(data["index_tree"], expected_index)
                self.assertTrue(data["source_unchanged"])
                self.assertEqual(formal_sources.source_facts(decoy), before)
                self.assertEqual({path.relative_to(decoy).as_posix(): path.read_bytes()
                                  for path in decoy.rglob("*") if path.is_file()}, protected)

    def test_unreadable_index_refuses_before_any_cargo(self) -> None:
        # E6: a real malformed private index -> Git128, unknown source and
        # zero Cargo body, not an empty source or successful stale HEAD plan.
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            self.runner_fixture(fixture)
            values = self.cargo_fixture(fixture, 'touch "$CARGO_TARGET_DIR/forbidden-cargo"\n')
            (fixture / ".git/index").write_bytes(b"malformed-index")
            result = self.required_shell("source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical", cwd=fixture, env=values, timeout=15)
            self.assertEqual(result.returncode, 128, result.stdout + result.stderr)
            self.assertIn("Cannot establish required working source", result.stderr)
            self.assertFalse((fixture / "target/forbidden-cargo").exists())

    def test_mutations_at_each_sequence_fence_stop_or_reject_execution(self) -> None:
        # E8: capture1/consistency2, then enter-list3/post-list4/enter-run5/
        # post-run6/terminal7. Mutate a real tracked file at each real shared
        # snapshot call, never replace the digest with a fake value. Exact
        # cause and executed-prefix lengths0/1/1/2/2 distinguish late-axis faults.
        import rust_test_plan
        original = rust_test_plan.working_subject
        for boundary, count in ((3, 0), (4, 1), (5, 1), (6, 2), (7, 2)):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                values = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CARGO_TARGET_DIR/calls"
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
''')
                directory = fixture / "target/evidence"
                directory.mkdir(parents=True)
                reads = []
                def mutate_then_read():
                    reads.append(1)
                    if len(reads) == boundary:
                        (fixture / "subject.txt").write_text("exact fence mutation\n")
                    return original()
                with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                     mock.patch.object(rust_test_plan, "ROOT", fixture), \
                     mock.patch.object(rust_test_plan, "working_subject", side_effect=mutate_then_read):
                    status = run_required(["-p", "critical", "--lib"], directory / "list.log", directory / "results.log", directory / "results.json")
                    errors = validate_receipt(directory / "list.log", directory / "results.log", status, directory / "results.json")["errors"]
                self.assertTrue(errors)
                self.assertGreaterEqual(len(reads), boundary)
                self.assertEqual((fixture / "subject.txt").read_text(), "exact fence mutation\n")
                calls = fixture / "target/calls"
                self.assertEqual(len(calls.read_text().splitlines()) if calls.exists() else 0, count)
                data = json.loads((directory / "results.json").read_text())
                self.assertEqual(data["status"], "failed")
                self.assertFalse(data["source_unchanged"])

    def test_initial_source_refusal_preserves_status_without_commands(self) -> None:
        # E6: the initial existing working-reader boundary refuses75/23/signal15
        # -> exact75/23/143 and zero child commands. The real malformed-index
        # case above owns actual Git128; these control the owner's failure code.
        for raw, expected in ((75, 75), (23, 23), (-15, 143)):
            with self.subTest(raw=raw), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                argv = [__file__, "--run", "--inventory", str(directory / "list.log"),
                        "--results", str(directory / "results.log"), "--receipt", str(directory / "results.json"),
                        "--", "-p", "critical", "--lib"]
                with mock.patch.object(sys, "argv", argv), \
                     mock.patch("rust_test_plan.working_subject", side_effect=subprocess.CalledProcessError(raw, ["git", "subject"])), \
                     mock.patch.object(subprocess, "run", side_effect=AssertionError("dependent Cargo started")):
                    self.assertEqual(main(), expected)

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux owned parent/PID observation")
    def test_whole_deadline_finalizes_but_dead_parent_leaves_partial_record(self) -> None:
        # E10/E12: the real existing deadline kills its ready inherited group.
        # With a live finalizer, timeout124 gets a failed terminal record. Killing
        # that exact parent shell prevents finalization: retain running/partial,
        # never claim it completed. The independent deadline still terminates
        # its owned children. GROUP_CODE publishes one complete readiness
        # document; failed observation/assertion still retires this fixture's
        # inherited group through its shared cleanup. Production deadlines
        # and their cleanup policy remain unchanged.
        from test_deadline import GROUP_CODE, cleanup_fixture_group
        for dead_parent in (False, True):
            with self.subTest(dead_parent=dead_parent), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                body = '''
case "$*" in *--list*) echo 'critical: test'; exit 0 ;; esac
exec ''' + shlex.quote(sys.executable) + " -c " + shlex.quote(GROUP_CODE) + ' "$CARGO_TARGET_DIR/group" 0 0\n'
                values = self.cargo_fixture(fixture, body)
                values.update(AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS="2",
                              DEADLINE_COORDINATE=str(fixture / "target/deadline.pid"))
                process = subprocess.Popen(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical"],
                                           cwd=fixture, env=values, stdin=subprocess.DEVNULL,
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    ready = fixture / "target/group.ready"
                    until = time.monotonic() + 5
                    # awaken-wait: the actual inherited child group publishes PIDs.
                    while not ready.exists() and process.poll() is None and time.monotonic() < until:
                        time.sleep(.01)
                    self.assertTrue(ready.exists(), "owned command did not become ready")
                    owned = json.loads(ready.read_text())
                    if dead_parent:
                        deadline_pid = int((fixture / "target/deadline.pid").read_text())
                        def parent(pid):
                            return int(next(line.split()[1] for line in Path(f"/proc/{pid}/status").read_text().splitlines() if line.startswith("PPid:")))
                        finalizer = parent(parent(deadline_pid))
                        self.assertEqual(Path(f"/proc/{finalizer}/cwd").resolve(), fixture)
                        os.kill(finalizer, signal.SIGTERM)
                    output = process.communicate(timeout=12)
                    self.assertEqual(process.returncode, 143 if dead_parent else 124, output)
                    data = json.loads(next((fixture / "target/test-timings").glob("rust-execution.*/results.json")).read_text())
                    self.assertEqual(data["status"], "running" if dead_parent else "failed")
                    if not dead_parent:
                        self.assertEqual(data["exit_code"], 124)
                    self.assertIn("ready", next((fixture / "target/test-timings").glob("rust-execution.*/results.log")).read_text())
                    for pid in owned.values():
                        state = Path(f"/proc/{pid}/stat")
                        self.assertTrue(not state.exists() or state.read_text().split()[2] == "Z",
                                        "owned child remained live after its deadline")
                finally:
                    cleanup_fixture_group(fixture / "target/group", process)

    @unittest.skipIf(os.name == "nt", "POSIX shell status/admission fixture")
    def test_required_shell_keeps_execution_status(self) -> None:
        # E5: actual libtest child23/75/signal15 with passing named output ->
        # preserve23/75/143, not the validator's boolean1. This exercises the
        # real public shell/deadline, using actual private Git source admission.
        for status in (23, 75, 143):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                environment = self.cargo_fixture(fixture, '''
case "$*" in
  *--list*) echo 'critical: test'; exit 0 ;;
esac
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
[ "$RESULT_STATUS" != 143 ] || kill -TERM "$$"
exit "$RESULT_STATUS"
''')
                environment['RESULT_STATUS'] = str(status)
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical -- --exact"],
                                        cwd=fixture, env=environment, capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)

    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
    def test_portability_entrypoint_requires_the_current_launch_adapter_selection(self) -> None:
        # P0 the real transitive closure includes source/execution/deadline
        # owners, preserves executable modes and uses a detached private index.
        # P1 the exact current owner lists and executes => continue to E2E.
        # P2 empty selection or P3 former owner => reject before E2E, even when
        # Cargo itself returns success. Exercise the actual entrypoint/helper.
        # P4 an unrelated prerequisite adds a required case => consume that
        # actual runner declaration, without copying its case list here. The
        # launch selection remains independently fixed by SELECTED below, so
        # this scaffolding cannot admit an empty or former-owner launch.
        selected = "sandbox_source::tests::configured_working_directory_drives_process_and_acp_from_one_view"
        for case in ("passing", "extra-floor", "empty", "former-owner"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                scripts = self.runner_fixture(fixture, "scripts/ci/check-trusted-host-portability.sh",
                                              "scripts/ci/_cargo_target.sh", "scripts/ci/_provider_environment.sh")
                entrypoint = scripts / "check-trusted-host-portability.sh"
                if case == "former-owner":
                    source = entrypoint.read_text(encoding="utf-8")
                    entrypoint.write_text(source.replace(
                        "run_required_rust_tests -p awaken-execution-composition --lib ",
                        "run_required_rust_tests -p awaken-session-executor --lib ", 1), encoding="utf-8")
                elif case == "extra-floor":
                    source = entrypoint.read_text(encoding="utf-8")
                    position = source.index("--require-case=")
                    entrypoint.write_text(source[:position] +
                                          "--require-case=future_portability_floor " +
                                          source[position:], encoding="utf-8")
                binary = fixture / "bin"
                binary.mkdir()
                environment = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CALL_LOG"
selection=$(python3 - "$@" <<'PY'
import json, os, sys
from pathlib import Path
command = ["cargo", *sys.argv[1:]]
receipts = Path(os.environ["CARGO_TARGET_DIR"]).glob("test-timings/rust-execution.*/results.json")
selection = ["portable_fixture"]
for receipt in sorted(receipts, key=lambda path: path.stat().st_mtime, reverse=True):
    document = json.loads(receipt.read_text())
    if document.get("status") == "running" and command in document.get("commands", []):
        selection = document["required_cases"] or selection
        break
print("\\n".join(selection))
PY
)
case "$*" in
  *configured_working_directory_drives_process_and_acp_from_one_view*)
    selection="$SELECTED"
    [[ " $* " == *" -p awaken-execution-composition "* && "$CASE" != empty ]] || selection=''
    ;;
esac
case "$*" in
  *--list*)
    if [ -n "$selection" ]; then
      while IFS= read -r name; do echo "$name: test"; done <<< "$selection"
    fi ;;
  *--show-output*)
    if [ -n "$selection" ]; then
      count=0
      while IFS= read -r name; do echo "test $name ... ok"; count=$((count + 1)); done <<< "$selection"
      echo "test result: ok. $count passed; 0 failed; 0 ignored;"
    else
      echo 'test result: ok. 0 passed; 0 failed; 0 ignored;'
    fi ;;
esac
exit 0
''')
                commands = {
                    "uname": "#!/bin/sh\necho MINGW64_NT\n",
                    "cygpath": '#!/bin/sh\ncase "$1" in\n-w) echo /owned/git-bash ;;\n-m) printf "%s\\n" "$2" ;;\n*) exit 2 ;;\nesac\n',
                    "node": '''#!/bin/sh
if [ "$1" = --input-type=module ]; then echo /owned/hand; else echo E2E >> "$CALL_LOG"; fi
''',
                }
                for name, source in commands.items():
                    command = binary / name
                    command.write_text(source, encoding="utf-8")
                    command.chmod(0o755)
                environment.update(CASE=case, SELECTED=selected, CALL_LOG=str(fixture / "target/calls"))
                result = subprocess.run(["bash", str(entrypoint)], cwd=fixture, env=environment,
                                        capture_output=True, text=True, timeout=90)
                passing = case in ("passing", "extra-floor")
                self.assertEqual(result.returncode == 0, passing, result.stdout + result.stderr)
                calls = (fixture / "target/calls").read_text().splitlines()
                if passing:
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --list", calls)
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --show-output", calls)
                    self.assertEqual(calls.count("E2E"), 5)
                else:
                    self.assertIn("required selection is empty", result.stderr)
                    self.assertNotIn("E2E", calls)

    def test_actual_required_runner_rejects_empty_ignored_skipped_and_failed_runs(self) -> None:
        # E0 both orchestration fixtures consume the actual complete script
        # closure and private Git; missing dependencies cannot fake a negative.
        # E1 same argv for listing/execution, including existing libtest args.
        # E2 zero selection, E3 ignored, E4 fixture skip, E5 execution failure,
        # E6 listing failure all fail. Preserve E6's status and never execute.
        for case in ("passing", "empty", "ignored", "skipped", "legacy-skipped", "failed", "list-failed"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                environment = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CALL_LOG"
case "$*" in
  *--list*) [ "$CASE" = empty ] || echo 'critical: test'; [ "$CASE" != list-failed ] || exit 47; exit 0 ;;
esac
if [ "$CASE" = empty ]; then
  echo 'test result: ok. 0 passed; 0 failed; 0 ignored;'; exit 0
elif [ "$CASE" = ignored ]; then
  echo 'test critical ... ignored'; echo 'test result: ok. 0 passed; 0 failed; 1 ignored;'; exit 0
fi
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
[ "$CASE" != skipped ] || echo 'SKIPPED PostgreSQL fixture: absent'
[ "$CASE" != legacy-skipped ] || echo 'skipping live Worker identity: AWAKEN_K8S_E2E=1 is required'
[ "$CASE" != failed ]
''')
                environment.update(CASE=case, CALL_LOG=str(fixture / "target/calls"))
                result = self.required_shell("source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical -- --exact", cwd=fixture, env=environment, timeout=15)
                self.assertEqual(result.returncode == 0, case == "passing", result.stdout + result.stderr)
                expected = ["test --locked -p critical --lib critical -- --exact --list"]
                if case == "list-failed":
                    self.assertEqual(result.returncode, 47)
                elif case != "empty":
                    expected.append("test --locked -p critical --lib critical -- --exact --show-output")
                self.assertEqual((fixture / "target/calls").read_text().splitlines(), expected)


    @unittest.skipIf(os.name == "nt", "systemd substrate qualification belongs to Linux")
    def test_sandbox_entrypoint_requires_the_current_systemd_case(self) -> None:
        # S1 exact process-startup case executes => this substrate passes. S2 empty/renamed
        # selection, S3 the former CLI owner, S4 ignored/skipped/failed
        # execution, S5 missing live-fixture opt-in => this substrate fails.
        # Run the actual entrypoint and source/list/run/deadline owners. Other
        # substrate adapters are deliberately absent, so the complete suite
        # must remain non-green in every orchestration-only fixture below.
        selected = "worker_transport::systemd::tests::live_systemd_unit_stop_survives_adapter_reopen"
        for case in ("passing", "empty", "former-owner", "renamed", "ignored",
                     "fixture-skip", "failed", "missing-env"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture, "scripts/e2e/sandbox_capability_suite.sh")
                entrypoint = fixture / "scripts/e2e/sandbox_capability_suite.sh"
                if case in ("former-owner", "missing-env"):
                    text = entrypoint.read_text(encoding="utf-8")
                    old, new = (("-p awaken-process-startup", "-p awaken-cli") if case == "former-owner"
                                else ("AWAKEN_SYSTEMD_WORKER_E2E=1 run", "run"))
                    self.assertIn(old, text)
                    entrypoint.write_text(text.replace(old, new, 1), encoding="utf-8")
                environment = self.cargo_fixture(fixture, r'''
printf '%s\n' "$*" >> "$CARGO_TARGET_DIR/calls"
[[ "$*" == *live_systemd_unit_stop_survives_adapter_reopen* ]] || exit 0
selection="$SELECTED"
[[ " $* " == *" -p awaken-process-startup "* && "$CASE" != empty ]] || selection=''
[[ "$CASE" != renamed ]] || selection=renamed_systemd_case
case "$*" in
  *--list*)
    [ -z "$selection" ] || echo "$selection: test"
    exit 0 ;;
esac
if [ -z "$selection" ]; then
  echo 'test result: ok. 0 passed; 0 failed; 0 ignored;'
elif [ "$CASE" = ignored ]; then
  echo "test $selection ... ignored"
  echo 'test result: ok. 0 passed; 0 failed; 1 ignored;'
elif [ "$CASE" = failed ]; then
  echo "test $selection ... FAILED"
  echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored;'
  exit 17
else
  echo "test $selection ... ok"
  echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
  if [ "${AWAKEN_SYSTEMD_WORKER_E2E:-}" != 1 ] || [ "$CASE" = fixture-skip ]; then
    echo 'skipping live systemd Worker: AWAKEN_SYSTEMD_WORKER_E2E=1 is required' >&2
  fi
fi
''')
                binary = fixture / "bin"
                for name, body in {"node": "exit 1", "docker": "exit 1", "podman": "exit 1",
                                   "systemctl": "exit 0", "systemd-run": "exit 0"}.items():
                    command = binary / name
                    command.write_text("#!/bin/sh\n" + body + "\n", encoding="utf-8")
                    command.chmod(0o755)
                environment.pop("AWAKEN_SYSTEMD_WORKER_E2E", None)
                environment.update(CASE=case, SELECTED=selected, AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS="10")
                result = subprocess.run(["bash", str(entrypoint), "--require-substrates"],
                                        cwd=fixture, env=environment, capture_output=True, text=True, timeout=30)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 1, output)  # other substrates are absent
                label = "systemd:exact-worker-exit-after-reopen"
                self.assertEqual(f"PASS: {label}" in output, case == "passing", output)
                self.assertEqual(f"FAIL: {label}" in output, case != "passing", output)
                calls = (fixture / "target/calls").read_text().splitlines()
                if case == "passing":
                    self.assertIn(f"test --locked -p awaken-process-startup --lib {selected} -- --exact --list", calls)
                    self.assertIn(f"test --locked -p awaken-process-startup --lib {selected} -- --exact --show-output", calls)
                elif case in ("empty", "former-owner"):
                    self.assertIn("required selection is empty", output)
                elif case == "renamed":
                    self.assertIn("required selection omits cases: " + selected, output)
                elif case in ("fixture-skip", "missing-env"):
                    self.assertIn("a selected test skipped its fixture or behavior", output)
                elif case == "failed":
                    self.assertIn("test command failed with status 17", output)

    def test_selection_and_execution_decision_table(self) -> None:
        # R1 selected identities + actual successes => pass. R2 zero/renamed/
        # missing/duplicated tests, ignored/failure, false summary, fixture skip
        # or process failure => reject. Equal counts with different names fail.
        inventory = "tests::one: test\ntests::two: test\n2 tests, 0 benchmarks\n"
        output = "test tests::one ... ok\ntest tests::two ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_evidence(inventory, output, 0)["errors"], [])
        variants = (
            ("", output, 0), (inventory, "", 0),
            (inventory, output.replace("tests::two ...", "tests::renamed ..."), 0),
            (inventory, output.replace("test tests::two ... ok\n", ""), 0),
            (inventory, output + "test tests::one ... ok\n", 0),
            (inventory, output.replace("two ... ok", "two ... ignored"), 0),
            (inventory, output.replace("two ... ok", "two ... FAILED"), 0),
            (inventory, output.replace("2 passed", "0 passed"), 0),
            (inventory, output.replace("0 ignored", "1 ignored"), 0),
            (inventory, output + "SKIPPED PostgreSQL fixture: absent\n", 0),
            (inventory, output + "[skip] no NATS reachable\n", 0),
            (inventory, output + "skipping live Worker identity: AWAKEN_K8S_E2E=1 is required\n", 0),
            (inventory, output + "Skipping live systemd Worker: AWAKEN_SYSTEMD_WORKER_E2E=1 is required\n", 0),
            (inventory, output, 101),
        )
        for listed, result, status in variants:
            with self.subTest(listed=listed, result=result, status=status):
                self.assertTrue(execution_evidence(listed, result, status)["errors"])

    def test_fixture_skip_markers_are_exact_line_start_tokens(self) -> None:
        # A fixture-return marker must reject an otherwise passing selected
        # test. Plain SKIP is the actual kernel FUSE fixture's spelling.
        # Ordinary words/status descriptions must not become skip events.
        inventory = "critical: test\n"
        output = "test critical ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        for marker in (
            "SKIP actual kernel cache wiring: fixture absent",
            "skip PostgreSQL fixture: absent",
            "SKIPPED PostgreSQL fixture: absent",
            "SKIPPING live Worker identity: not configured",
            "[skip] no NATS reachable",
            "[SKIP] FUSE unavailable",
        ):
            with self.subTest(marker=marker):
                self.assertIn(
                    "a selected test skipped its fixture or behavior",
                    execution_evidence(inventory, output + marker + "\n", 0)["errors"],
                )
        for ordinary in (
            "skipper owns the connection",
            "SKIPPER is a fixture name",
            "skippingly is an ordinary word",
            "skipped_count=0",
            "skip_count=0",
            "skipValidation=false",
            "diagnostic: SKIP is a status label",
            "all selected tests ran without skips",
        ):
            with self.subTest(ordinary=ordinary):
                self.assertEqual(
                    execution_evidence(inventory, output + ordinary + "\n", 0)["errors"], [],
                )

    def test_indented_fixture_skip_markers_reject_success_status(self) -> None:
        inventory = "critical: test\n"
        output = "test critical ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        for indent in (" ", "  ", "\t"):
            for marker in ("SKIP fixture absent", "SKIPPED fixture absent",
                           "SKIPPING fixture absent", "[skip] fixture absent"):
                with self.subTest(indent=indent, marker=marker):
                    self.assertIn(
                        "a selected test skipped its fixture or behavior",
                        execution_evidence(inventory, output + indent + marker + "\n", 0)["errors"],
                    )
        for ordinary in ("  skipper owns a connection", "\tskipped_count=0",
                         "  skip_count=0", "  [skipper] diagnostic",
                         "  diagnostic: SKIP is a status label"):
            with self.subTest(ordinary=ordinary):
                self.assertEqual(execution_evidence(inventory, output + ordinary + "\n", 0)["errors"], [])

    def test_same_name_in_two_binaries_retains_occurrence_count(self) -> None:
        inventory = "same: test\nsame: test\n"
        output = ("test same ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n") * 2
        self.assertEqual(execution_evidence(inventory, output, 0)["errors"], [])
        self.assertTrue(execution_evidence(inventory, output[:len(output) // 2], 0)["errors"])

    def test_expected_panic_annotation_preserves_identity_and_failure_checks(self) -> None:
        # The annotation is libtest status syntax, never a second test name.
        # A success qualifies only the listed identity and exact summary;
        # missing panic, ignore, rename and duplicate results still fail.
        inventory = "expected: test\n"
        output = "test expected - should panic ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_evidence(inventory, output, 0)["errors"], [])
        for result in [output.replace(" ... ok", " ... FAILED"),
                       output.replace(" ... ok", " ... ignored"),
                       output.replace("test expected", "test other"),
                       output + "test expected - should panic ... ok\n"]:
            self.assertTrue(execution_evidence(inventory, result, 0)["errors"])

    def test_actual_libtest_expected_panic_format_is_qualified(self) -> None:
        # Drive the real compiler and libtest boundary. Expected panic succeeds;
        # a should_panic body returning normally fails without losing identity.
        with tempfile.TemporaryDirectory(prefix="awaken-expected-panic-") as temporary:
            root = Path(temporary)
            source = root / "fixture.rs"
            source.write_text('''#[test] #[should_panic] fn expected() { panic!("expected"); }
#[test] #[should_panic] fn missing() {}
''', encoding="utf-8")
            binary = root / ("fixture.exe" if os.name == "nt" else "fixture")
            compiled = subprocess.run(["rustc", "--test", str(source), "-o", str(binary)],
                                      capture_output=True, text=True, encoding="utf-8")
            self.assertEqual(compiled.returncode, 0, compiled.stdout + compiled.stderr)
            for case, succeeds in [("expected", True), ("missing", False)]:
                listed = subprocess.run([str(binary), "--exact", case, "--list"],
                                        check=True, capture_output=True, text=True).stdout
                executed = subprocess.run([str(binary), "--exact", case, "--show-output"],
                                          capture_output=True, text=True)
                evidence = execution_evidence(listed, executed.stdout, executed.returncode, [case])
                self.assertEqual(not evidence["errors"], succeeds)
                self.assertEqual([name for name, _ in evidence["executed_cases"]], [case])

    def test_missing_backend_is_rejected_even_when_every_listed_test_passes(self) -> None:
        # F1 the declared SQLite floor is present and executed -> qualify.
        # F2 adding a missing PostgreSQL obligation -> refuse despite every
        # selected test passing. Floors belong to the actual domain caller.
        inventory = "sqlite: test\n"
        output = "test sqlite ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_evidence(inventory, output, 0, ["sqlite"])["errors"], [])
        self.assertTrue(execution_evidence(inventory, output, 0, ["sqlite", "postgres"])["errors"])

    def test_success_count_and_receipt_share_the_validated_inventory_observation(self) -> None:
        # V1 named occurrences x an existing/offline floor present -> pass,
        # count2 and receipt preserve the exact selected/executed occurrences.
        # V2 an additional offline floor missing -> reject even with child0.
        # V3 a second inventory read would fail: validation, record and printed
        # count must all use the first observation. Independent phase/source
        # fences remain; this is not a cache between list, run and terminal.
        import rust_test_plan
        original_read = Path.read_text
        for receipt_mode in (False, True):
            for floor_present in (False, True):
                with self.subTest(receipt=receipt_mode, floor=floor_present), tempfile.TemporaryDirectory() as temporary:
                    fixture = Path(temporary)
                    self.runner_fixture(fixture)
                    values = self.cargo_fixture(fixture, '''
case "$*" in *--list*) printf 'critical: test\\ncritical: test\\n'; exit 0 ;; esac
printf 'test critical ... ok\\ntest critical ... ok\\n'
echo 'test result: ok. 2 passed; 0 failed; 0 ignored;'
''')
                    values.update(AWAKEN_TEST_REQUIRE_POSTGRES="1", AWAKEN_TEST_DATABASE_URL="fixture-only")
                    directory = fixture / "target/evidence"
                    directory.mkdir(parents=True)
                    inventory, results, receipt = [directory / name for name in ("list.log", "results.log", "results.json")]
                    reads = Counter()
                    def read_once(path, *args, **kwargs):
                        if path in (inventory, results):
                            reads[path] += 1
                            if reads[path] != 1:
                                raise AssertionError("validated evidence was read again")
                        return original_read(path, *args, **kwargs)
                    with mock.patch.dict(os.environ, values, clear=True), mock.patch(__name__ + ".ROOT", fixture), \
                         mock.patch.object(rust_test_plan, "ROOT", fixture):
                        self.assertEqual(run_required(["-p", "critical", "--lib", "--require-case=critical"],
                                                      inventory, results, receipt), 0)
                        before = receipt.read_bytes()
                        argv = [__file__, "--inventory", str(inventory), "--results", str(results),
                                "--exit-code", "0", "--require-case", "critical" if floor_present else "missing"]
                        if receipt_mode:
                            argv += ["--receipt", str(receipt)]
                        output, diagnostics = io.StringIO(), io.StringIO()
                        with mock.patch.object(sys, "argv", argv), mock.patch.object(Path, "read_text", read_once), \
                             mock.patch(__name__ + "._listed_selection", wraps=_listed_selection) as selection, \
                             contextlib.redirect_stdout(output), contextlib.redirect_stderr(diagnostics):
                            status = main()
                        self.assertEqual(status, 0 if floor_present else 1)
                        self.assertEqual(reads, Counter({inventory: 1, results: 1}))
                        selection.assert_called_once()
                        if floor_present:
                            self.assertIn("2 selected tests passed without skips", output.getvalue())
                        else:
                            self.assertIn("required selection omits cases: missing", diagnostics.getvalue())
                            self.assertNotIn("passed without skips", output.getvalue())
                        if receipt_mode:
                            recorded = json.loads(receipt.read_text())
                            self.assertEqual(recorded["selected_cases"], ["critical", "critical"])
                            self.assertEqual(recorded["executed_cases"], [["critical", "ok"], ["critical", "ok"]])
                            self.assertEqual(recorded["command_exit_code"], 0)
                            self.assertEqual(recorded["exit_code"], status)
                            self.assertEqual(recorded["status"], "passed" if floor_present else "failed")
                            self.assertTrue(recorded["postgres_fixture_required"])
                            self.assertTrue(recorded["postgres_fixture_configured"])
                            self.assertFalse(recorded["full_gate"])
                        else:
                            self.assertEqual(receipt.read_bytes(), before)



    @unittest.skipUnless(os.name == "nt", "real directory junctions require Windows")
    def test_prepared_build_log_uses_physical_evidence_directory_and_rejects_escapes(self) -> None:
        # Both directions through a cache-root junction must identify the same
        # evidence directory. Equal bytes in a sibling/outside directory, or a
        # nested junction escaping that directory, must never satisfy ownership.
        # Only receipt/path validation runs here; this is not a Cargo build fact.
        from _required_build_context import build_context
        arguments = ["-p", "critical", "--lib"]
        command = required_build_command(arguments)
        context = build_context(ROOT, arguments)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            physical = root / "physical"; physical.mkdir()
            evidence = physical / "evidence"; evidence.mkdir()
            sibling = physical / "sibling"; sibling.mkdir()
            outside = root / "outside"; outside.mkdir()
            alias = root / "alias"
            escape = evidence / "escape"
            links = []
            try:
                for link, target in ((alias, physical), (escape, outside)):
                    subprocess.run(["cmd.exe", "/d", "/c", "mklink", "/J", str(link), str(target)],
                                   check=True, capture_output=True)
                    links.append(link)
                    self.assertTrue(link.is_junction())
                    self.assertEqual(link.resolve(strict=True), target.resolve(strict=True))
                content = b"retained build output\n"
                for directory in (evidence, sibling, outside):
                    (directory / "build.log").write_bytes(content)
                build = evidence / "build.json"
                document = {"kind": "required-libtest-build", "status": "built", "exit_code": 0,
                            "source_unchanged": True, "context_unchanged": True,
                            "source_digest": "path-fixture", "arguments": arguments,
                            "command": command, "compile_context": context,
                            "rows": [{"command": command, "exit_code": 0}],
                            "build_log_sha256": hashlib.sha256(content).hexdigest()}
                rows = (
                    ("physical", build, evidence / "build.log", True),
                    ("alias-log", build, alias / "evidence/build.log", True),
                    ("alias-receipt", alias / "evidence/build.json", evidence / "build.log", True),
                    ("both-aliases", alias / "evidence/build.json", alias / "evidence/build.log", True),
                    ("sibling", build, sibling / "build.log", False),
                    ("outside", build, outside / "build.log", False),
                    ("nested-junction-escape", build, escape / "build.log", False),
                )
                for name, receipt, log, admitted in rows:
                    with self.subTest(row=name):
                        build.write_text(json.dumps(dict(document, build_log=str(log))), encoding="utf-8")
                        if admitted:
                            result = verify_preparation(receipt, arguments, "path-fixture")
                            self.assertEqual(result["sha256"], hashlib.sha256(build.read_bytes()).hexdigest())
                        else:
                            with self.assertRaisesRegex(ValueError, "share its evidence directory"):
                                verify_preparation(receipt, arguments, "path-fixture")
                build.write_text(json.dumps(dict(document, build_log=str(alias / "evidence/build.log"))), encoding="utf-8")
                (evidence / "build.log").write_bytes(b"changed output\n")
                with self.assertRaisesRegex(ValueError, "Required build log changed"):
                    verify_preparation(build, arguments, "path-fixture")
                (evidence / "build.log").unlink()
                with self.assertRaises(FileNotFoundError):
                    verify_preparation(build, arguments, "path-fixture")
            finally:
                # Remove only the links this fixture created, never their targets.
                for link in reversed(links):
                    if link.is_junction():
                        link.rmdir()


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--require-case", action="append", default=[])
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--build-log", type=Path)
    parser.add_argument("--prepared", type=Path)
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.self_test:
        if args.inventory or args.results or args.exit_code is not None or args.run or args.prepare or args.build_log or args.prepared or args.receipt or args.arguments or args.require_case:
            parser.error("--self-test cannot be combined with execution arguments")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)
        return int(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
    if args.prepare:
        if (args.run or args.inventory or args.results or args.exit_code is not None
                or args.require_case or args.prepared or args.build_log is None or args.receipt is None):
            parser.error("--prepare requires --build-log and --receipt, without execution arguments")
        arguments = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
        if not arguments:
            parser.error("--prepare requires the focused Cargo test arguments after --")
        try:
            return prepare_required(arguments, args.build_log, args.receipt)
        except argparse.ArgumentError as error:
            parser.error(str(error))
        except subprocess.CalledProcessError as error:
            print("Cannot establish required working source (build): " + str(error), file=sys.stderr)
            return error.returncode if error.returncode >= 0 else 128 - error.returncode
        except (OSError, ValueError, KeyError, TypeError) as error:
            print("Cannot prepare required build: " + str(error), file=sys.stderr)
            return 1
    if args.build_log or (args.prepared and not args.run):
        parser.error("--build-log requires --prepare; --prepared requires --run")
    if args.run:
        if args.inventory is None or args.results is None or args.receipt is None or args.exit_code is not None:
            parser.error("--run requires --inventory, --results and --receipt, without --exit-code")
        if args.require_case:
            parser.error("--run floors belong to the focused arguments as --require-case=NAME")
        arguments = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
        if not arguments:
            parser.error("--run requires the focused Cargo test arguments after --")
        try:
            return run_required(arguments, args.inventory, args.results, args.receipt, args.prepared)
        except argparse.ArgumentError as error:
            parser.error(str(error))
        except subprocess.CalledProcessError as error:
            print(f"Cannot establish required working source: {error}", file=sys.stderr)
            return error.returncode if error.returncode >= 0 else 128 - error.returncode
        except (OSError, ValueError, KeyError, TypeError) as error:
            print(f"Cannot establish required execution: {error}", file=sys.stderr)
            return 1
    if args.arguments:
        parser.error("Cargo arguments require --run")
    if args.inventory is None or args.results is None or args.exit_code is None:
        parser.error("--inventory, --results and --exit-code are required")
    observed = {}
    try:
        observed = (validate_receipt(args.inventory, args.results, args.exit_code, args.receipt, args.require_case) if args.receipt else
                    execution_evidence(args.inventory.read_text(encoding="utf-8"), args.results.read_text(encoding="utf-8"),
                                       args.exit_code, args.require_case))
        errors = observed["errors"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        errors = ["Cannot retain required execution evidence: " + str(error)]
    for error in errors:
        print(f"Rust execution: {error}", file=sys.stderr)
    if not errors:
        print(f"Rust execution: {len(observed['selected_cases'])} selected tests passed without skips")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
