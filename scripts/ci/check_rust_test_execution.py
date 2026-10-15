#!/usr/bin/env python3
"""Qualify a required libtest run against its independently listed selection."""
from __future__ import annotations

import argparse
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
INPUT_GLOBS = ("scripts/ci/**",)

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
            if argument == "--list" or argument.split("=", 1)[0] == "--format":
                raise ValueError("required test execution owns list/output format")
            cargo_arguments.append(argument)
    boundary = cargo_arguments.index("--") if "--" in cargo_arguments else len(cargo_arguments)
    locked = [] if "--locked" in cargo_arguments[:boundary] else ["--locked"]
    base = ["cargo", "test", *locked, *cargo_arguments]
    if boundary == len(cargo_arguments):
        base.append("--")
    return [[*base, "--list"], [*base, "--show-output"]], required


def _write_receipt(path: Path, document: dict) -> None:
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def run_required(arguments: list[str], inventory: Path, results: Path, receipt: Path) -> int:
    """Adapt two focused commands to the existing sequence, never run a loop."""
    from crate_test import execute_commands, native_command
    from rust_test_plan import source_snapshot, working_subject

    try:
        commands, required = required_commands(arguments)
    except ValueError as error:
        raise argparse.ArgumentError(None, str(error)) from error
    if inventory.parent != results.parent or receipt.parent != results.parent or receipt.exists():
        raise ValueError("required execution needs one fresh evidence directory")
    inventory.touch()
    results.touch()
    digest, facts = working_subject()
    index = facts["index_tree"].decode().strip()
    if source_snapshot()[0] != digest:
        raise ValueError("Source changed while binding required execution facts")
    (receipt.parent / "working-tree.patch").write_bytes(facts["patch"])
    document = {"schema_version": 1, "kind": "required-libtest", "full_gate": False,
                "source_kind": "working-tree", "revision": facts["revision"].decode().strip(),
                "source_digest": digest, "index_tree": index,
                "arguments": arguments, "required_cases": required, "commands": commands,
                "list_command": commands[0], "test_command": commands[1],
                "selected_cases": [], "executed_cases": [],
                "postgres_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_POSTGRES") == "1",
                "postgres_fixture_configured": "AWAKEN_TEST_DATABASE_URL" in os.environ,
                "status": "running", "rows": [], "errors": []}
    _write_receipt(receipt, document)
    def verify():
        if source_snapshot()[0] != digest:
            raise ValueError("Source changed before or during required execution")
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
    return execute_commands(commands, dict(os.environ), root=ROOT, verify=verify, record=record, run=run)[0]


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
    @staticmethod
    def runner_fixture(fixture: Path, *entrypoint_inputs: str) -> Path:
        from formal_plan import script_closure
        from formal_sources import detached_git_environment
        root = Path(__file__).resolve().parents[2]
        inputs = script_closure(root, ("scripts/ci/_rust_test_execution.sh",
                                      "scripts/ci/check_rust_test_execution.py", *entrypoint_inputs))
        for relative in sorted(inputs):
            target = fixture / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / relative, target)
        (fixture / ".gitignore").write_text("target/\n", encoding="utf-8")
        (fixture / "subject.txt").write_text("original source\n", encoding="utf-8")
        values = detached_git_environment()
        for arguments in (("init", "--quiet"), ("add", "."),
                          ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
            subprocess.run(["git", "-C", str(fixture), *arguments], env=values, check=True)
        return fixture / "scripts/ci"

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
  entry=$(command -v "cargo-$1"); exec "$entry" "$@"
fi
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

    @unittest.skipIf(os.name == "nt", "actual POSIX libtest boundary fixture")
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
            result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical --require-case=critical"],
                                    cwd=fixture, env=values, capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("required selection omits cases: critical", result.stderr)
            self.assertEqual(len((fixture / "target/calls").read_text().splitlines()), 1)

    @unittest.skipIf(os.name == "nt", "private Git/POSIX command fixture")
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
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical"],
                                        cwd=fixture, env=values, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("Source changed", result.stderr)
                self.assertEqual(len((fixture / "target/calls").read_text().splitlines()), 1)
                if cause == "index":
                    import formal_sources
                    self.assertEqual(formal_sources.git_output(fixture, "diff", "--binary", "HEAD"), b"")
                data = json.loads(next((fixture / "target/test-timings").glob("rust-execution.*/results.json")).read_text())
                self.assertEqual(data["status"], "failed")
                self.assertFalse(data["source_unchanged"])

    @unittest.skipIf(os.name == "nt", "private hook/index admission fixture")
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
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical"],
                                        cwd=own, env=values, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                data = json.loads(next((own / "target/test-timings").glob("rust-execution.*/results.json")).read_text())
                self.assertEqual(data["index_tree"], expected_index)
                self.assertTrue(data["source_unchanged"])
                self.assertEqual(formal_sources.source_facts(decoy), before)
                self.assertEqual({path.relative_to(decoy).as_posix(): path.read_bytes()
                                  for path in decoy.rglob("*") if path.is_file()}, protected)

    @unittest.skipIf(os.name == "nt", "private Git index refusal")
    def test_unreadable_index_refuses_before_any_cargo(self) -> None:
        # E6: a real malformed private index -> Git128, unknown source and
        # zero Cargo body, not an empty source or successful stale HEAD plan.
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            self.runner_fixture(fixture)
            values = self.cargo_fixture(fixture, 'touch "$CARGO_TARGET_DIR/forbidden-cargo"\n')
            (fixture / ".git/index").write_bytes(b"malformed-index")
            result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical"],
                                    cwd=fixture, env=values, capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 128, result.stdout + result.stderr)
            self.assertIn("Cannot establish required working source", result.stderr)
            self.assertFalse((fixture / "target/forbidden-cargo").exists())

    @unittest.skipIf(os.name == "nt", "real private source boundaries")
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
        # its owned children; no new signal/cleanup mechanism is installed.
        from test_deadline import GROUP_CODE
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
                                           cwd=fixture, env=values, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
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
        selected = "sandbox_source::tests::configured_working_directory_drives_process_and_acp_from_one_view"
        for case in ("passing", "empty", "former-owner"):
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
                binary = fixture / "bin"
                binary.mkdir()
                environment = self.cargo_fixture(fixture, '''
printf '%s\\n' "$*" >> "$CALL_LOG"
selection=portable_fixture
case "$*" in
  *configured_working_directory_drives_process_and_acp_from_one_view*)
    selection="$SELECTED"
    [[ " $* " == *" -p awaken-execution-composition "* && "$CASE" != empty ]] || selection=''
    ;;
esac
case "$*" in
  *--list*) [ -z "$selection" ] || echo "$selection: test" ;;
  *--show-output*)
    if [ -n "$selection" ]; then
      echo "test $selection ... ok"
      echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
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
                self.assertEqual(result.returncode == 0, case == "passing", result.stdout + result.stderr)
                calls = (fixture / "target/calls").read_text().splitlines()
                if case == "passing":
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --list", calls)
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --show-output", calls)
                    self.assertEqual(calls.count("E2E"), 5)
                else:
                    self.assertIn("required selection is empty", result.stderr)
                    self.assertNotIn("E2E", calls)

    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
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
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical -- --exact"], cwd=fixture, env=environment, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode == 0, case == "passing", result.stdout + result.stderr)
                expected = ["test --locked -p critical --lib critical -- --exact --list"]
                if case == "list-failed":
                    self.assertEqual(result.returncode, 47)
                elif case != "empty":
                    expected.append("test --locked -p critical --lib critical -- --exact --show-output")
                self.assertEqual((fixture / "target/calls").read_text().splitlines(), expected)

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



def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--require-case", action="append", default=[])
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.self_test:
        if args.inventory or args.results or args.exit_code is not None or args.run or args.receipt or args.arguments or args.require_case:
            parser.error("--self-test cannot be combined with execution arguments")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)
        return int(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
    if args.run:
        if args.inventory is None or args.results is None or args.receipt is None or args.exit_code is not None:
            parser.error("--run requires --inventory, --results and --receipt, without --exit-code")
        if args.require_case:
            parser.error("--run floors belong to the focused arguments as --require-case=NAME")
        arguments = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
        if not arguments:
            parser.error("--run requires the focused Cargo test arguments after --")
        try:
            return run_required(arguments, args.inventory, args.results, args.receipt)
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
