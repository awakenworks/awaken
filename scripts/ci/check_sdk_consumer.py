#!/usr/bin/env python3
"""Compile and execute the independent SDK consumer without workspace features."""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import crate_test
import formal_sources
import rust_test_plan
from generate_hooks import gate_mode_source, glob_matches

# The independent manifest and its path dependencies are a different feature
# graph from the workspace. The existing Rust gate/generator owns their union.
INPUT_GLOBS = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**", "crates/**",
    "examples/sdk-consumer/**", "scripts/ci/*.py", "scripts/ci/check-rust.sh",
)

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "examples/sdk-consumer/Cargo.toml"
EXPECTED = {
    "single_turn_exposes_identity_state_and_committed_output",
    "multiple_turns_share_one_store_and_thread",
    "one_typed_tool_definition_supplies_schema_and_execution",
    "streaming_delivers_progress_and_commits_complete_unicode_output",
    "cancelled_input_never_enters_the_model",
    "asynchronous_approval_resumes_the_committed_ticket",
    "filesystem_restart_reads_receipt_and_replays_terminal_without_a_model",
}


def admission_errors(owner: str, gate: str) -> list[str]:
    errors = []
    triggers = re.findall(r'(?m)^\s*glob: "([^"]+)"\s*$', owner)
    for path in ("examples/sdk-consumer/Cargo.toml", "examples/sdk-consumer/Cargo.lock",
                 "examples/sdk-consumer/src/main.rs", "examples/sdk-consumer/tests/tasks.rs",
                 "scripts/ci/check_sdk_consumer.py"):
        if len(triggers) != 1 or not glob_matches(triggers[0], path):
            errors.append(f"Rust pre-commit must include the independent SDK consumer input: {path}")
    for mode in ("--quick", "--push", "--full"):
        try:
            source = gate_mode_source(gate, mode)
        except SystemExit as error:
            errors.append(f"SDK consumer gate mode cannot be admitted: {error}")
            break
        calls = re.findall(r'(?m)^\s*python3 scripts/ci/check_sdk_consumer\.py\s*$', source)
        if len(calls) != 1:
            errors.append(f"Rust gate {mode} must execute the independent SDK consumer exactly once")
    return errors


def validate_task_evidence(output: str) -> None:
    executed = re.findall(r"^test ([a-z_]+) \.\.\. ok$", output, re.MULTILINE)
    summary = f"{len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    if set(executed) != EXPECTED or len(executed) != len(EXPECTED) or summary not in output:
        raise RuntimeError(f"SDK consumer task evidence is incomplete: expected={sorted(EXPECTED)}, executed={sorted(executed)}")


def self_test() -> None:
    # S1 real invocation in the shared prefix -> all judging modes; S2 absent,
    # duplicate, comment or self-test-only invocation -> refusal. The canonical
    # mode parser, not substring occurrence, owns the executable branch view.
    owner = 'glob: "{examples/sdk-consumer/**,scripts/ci/check_sdk_consumer.py}"'
    invocation = "python3 scripts/ci/check_sdk_consumer.py"
    gate = ('mode="${1:---full}"\ncase "$mode" in\n'
            '  --quick|--push|--full)\n    ;;\nesac\n' + invocation + '\n'
            'case "$mode" in\n  --quick|--push)\n    true\n    ;;\n'
            '  --full)\n    true\n    ;;\nesac\n')
    assert not admission_errors(owner, gate)
    assert admission_errors(owner.replace("examples/sdk-consumer/**,", ""), gate)
    for changed in (gate.replace(invocation, "true"), gate + invocation + '\n',
                    gate.replace(invocation, '# ' + invocation),
                    gate.replace(invocation, invocation + " --self-test"),
                    gate.replace(invocation, 'if [ "$mode" = "--push" ]; then\n' + invocation + '\nfi'),
                    gate.replace(invocation, 'if [ "$mode" = "--full" ]; then\n' + invocation + '\nfi')):
        assert admission_errors(owner, changed)
    passing = "\n".join(f"test {name} ... ok" for name in sorted(EXPECTED))
    summary = "\ntest result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    validate_task_evidence(passing + summary)
    for invalid in (
        passing.split("\n", 1)[1] + summary,
        passing + "\n" + passing.split("\n", 1)[0] + summary,
        passing + summary.replace("0 ignored", "1 ignored"),
        passing + summary.replace("0 filtered out", "1 filtered out"),
        passing.replace(" ... ok", " ... FAILED", 1) + summary,
    ):
        try:
            validate_task_evidence(invalid)
        except RuntimeError:
            continue
        raise AssertionError("incomplete consumer execution evidence was accepted")


def run_consumer() -> int:
    """Keep SDK admission policy; the existing sequence owns source and status."""
    commands = [
        ["cargo", "metadata", "--manifest-path", str(MANIFEST), "--no-deps", "--format-version", "1", "--locked"],
        ["cargo", "test", "--manifest-path", str(MANIFEST), "--locked", "--test", "tasks", "--", "--test-threads=1"],
    ]
    subject = None
    completed = None

    def verify() -> None:
        nonlocal subject
        observed = rust_test_plan.source_snapshot()[0]
        if subject is None:
            subject = observed
        elif observed != subject:
            raise ValueError("SDK consumer source changed before or during execution")

    def capture(command, **arguments):
        # Only preserve the existing stdio contract. No retry, deadline, process
        # manager or command loop: Cargo admission and lifecycle stay upstream.
        nonlocal completed
        completed = None
        completed = subprocess.run(command, capture_output=True, text=True, **arguments)
        return completed

    def record(rows) -> None:
        if completed is None:
            return
        if rows[-1]["command"][1] == "test":
            sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        if rows[-1]["exit_code"]:
            return
        try:
            if rows[-1]["command"][1] == "metadata":
                data = json.loads(completed.stdout)
                if Path(data["workspace_root"]).resolve() != MANIFEST.parent.resolve() or len(data["workspace_members"]) != 1:
                    raise ValueError("SDK consumer must be its own one-member workspace")
                if any(dependency["name"] == "awaken-runtime-examples" for package in data["packages"] for dependency in package["dependencies"]):
                    raise ValueError("consumer cannot depend on the teaching fixture prelude")
            else:
                validate_task_evidence(completed.stdout)
        except (KeyError, TypeError, RuntimeError) as error:
            # The sole sequence already handles ValueError/OSError and keeps
            # original23/75/143 above secondary source/output/validator faults.
            raise ValueError(str(error)) from error

    status, _ = crate_test.execute_commands(commands, crate_test.environment(ROOT),
                                           root=ROOT, verify=verify, record=record, run=capture)
    return status


class SdkConsumerTests(unittest.TestCase):
    """Private Git subjects; Cargo calls below are controlled at the sole IO edge."""

    @staticmethod
    def git(root: Path, *arguments: str) -> None:
        subprocess.run(["git", "-C", str(root), *arguments], check=True,
                       env=formal_sources.detached_git_environment(), capture_output=True)

    def repository(self, outer: Path, name: str = "subject") -> Path:
        root = outer / name
        root.mkdir()
        if name == "subject":
            from formal_plan import script_closure
            for relative in script_closure(ROOT, ("scripts/ci/check_sdk_consumer.py",)):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / relative, destination)
            manifest = root / "examples/sdk-consumer/Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text('[package]\nname="sdk"\nversion="0.0.0"\n[workspace]\n')
        (root / "tracked.txt").write_text("before\n")
        self.git(root, "init", "--quiet")
        self.git(root, "config", "user.name", "SDK source fixture")
        self.git(root, "config", "user.email", "fixture@example.invalid")
        self.git(root, "config", "commit.gpgsign", "false")
        self.git(root, "config", "core.hooksPath", str(outer / "fixture-hooks"))
        self.git(root, "add", ".")
        self.git(root, "commit", "--quiet", "-m", "private source")
        return root

    def execute(self, root: Path, *, metadata_status=0, test_status=0,
                mutation="", fault="", metadata_cause="", output_cause="", phase="metadata") -> tuple[int, list]:
        manifest = root / "examples/sdk-consumer/Cargo.toml"
        metadata = {"workspace_root": str(manifest.parent), "workspace_members": ["sdk"],
                    "packages": [{"dependencies": []}]}
        if metadata_cause == "workspace":
            metadata["workspace_root"] = str(root)
        elif metadata_cause == "members":
            metadata["workspace_members"] = []
        elif metadata_cause == "prelude":
            metadata["packages"][0]["dependencies"] = [{"name": "awaken-runtime-examples"}]
        elif metadata_cause == "shape":
            metadata = {}
        passing = "\n".join(f"test {name} ... ok" for name in sorted(EXPECTED))
        output = passing + "\ntest result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        if output_cause == "missing":
            output = output.split("\n", 1)[1]
        calls = []
        actual_run = subprocess.run

        def run(command, **arguments):
            if command[0] != "cargo":
                return actual_run(command, **arguments)
            self.assertEqual(arguments["cwd"], root)
            self.assertEqual(arguments["env"]["CARGO_BUILD_BUILD_DIR"], arguments["env"]["CARGO_TARGET_DIR"])
            current_phase = command[1]
            calls.append(command)
            if mutation and current_phase == phase:
                if mutation == "working":
                    (root / "tracked.txt").write_text("after\n")
                elif mutation == "index":
                    (root / "tracked.txt").write_text("indexed\n")
                    self.git(root, "add", "tracked.txt")
                    (root / "tracked.txt").write_text("before\n")
                elif mutation == "untracked":
                    (root / "untracked.txt").write_text("after\n")
            if fault == "spawn":
                raise OSError("exact SDK spawn refusal")
            status = metadata_status if current_phase == "metadata" else test_status
            stdout = ("broken-json" if metadata_cause == "json" else json.dumps(metadata)) if current_phase == "metadata" else output
            stderr = "SDK captured diagnostic\n" if current_phase == phase else ""
            return subprocess.CompletedProcess(command, status, stdout, stderr)

        class Output(io.StringIO):
            def write(self, value):
                if fault == "output" and value.startswith("SDK captured"):
                    raise OSError("exact SDK evidence output refusal")
                return super().write(value)

        with mock.patch.object(sys.modules[__name__], "ROOT", root), \
                mock.patch.object(sys.modules[__name__], "MANIFEST", manifest), \
                mock.patch.object(rust_test_plan, "ROOT", root), \
                mock.patch.object(subprocess, "run", side_effect=run), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(Output()):
            return run_consumer(), calls

    def test_original_argv_and_environment_reach_both_commands(self):
        # E1 valid independent metadata + all seven tasks -> 0. Retain the exact
        # manifest/no-deps/locked/test target/literal -- argv and BUILD=TARGET;
        # no workspace full features or a companion may enter this SDK graph.
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary))
            status, calls = self.execute(root)
            manifest = str(root / "examples/sdk-consumer/Cargo.toml")
            self.assertEqual(status, 0)
            self.assertEqual(calls, [
                ["cargo", "metadata", "--manifest-path", manifest, "--no-deps", "--format-version", "1", "--locked"],
                ["cargo", "test", "--manifest-path", manifest, "--locked", "--test", "tasks", "--", "--test-threads=1"],
            ])

    def test_original_failure_outranks_source_or_output_faults(self):
        # E2 metadata23/75/-15 -> 23/75/143 and no test. E3 each phase crossed
        # with real post-source mutation or writer refusal keeps that failure;
        # successful commands become1. Last-command source changes cannot pass.
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary))
            for phase in ("metadata", "test"):
                for raw, expected in ((0, 1), (23, 23), (75, 75), (-15, 143)):
                    for cause in ("source", "output"):
                        with self.subTest(phase=phase, raw=raw, cause=cause):
                            status, calls = self.execute(root, **{phase + "_status": raw}, phase=phase,
                                mutation="working" if cause == "source" else "",
                                fault="output" if cause == "output" else "")
                            self.assertEqual((status, len(calls)), (expected, 1 if phase == "metadata" else 2))
                            (root / "tracked.txt").write_text("before\n")
            for raw, expected in ((0, 0), (23, 23), (75, 75), (-15, 143)):
                with self.subTest(test_status=raw):
                    status, calls = self.execute(root, test_status=raw)
                    self.assertEqual((status, len(calls)), (expected, 2))

    def test_metadata_or_task_validator_refusal_stops_the_sequence(self):
        # E4 zero + malformed JSON/shape/wrong workspace/member/prelude ->1,
        # metadata only. E5 zero + missing named task ->1 after the test. These
        # are SDK semantic validators, not a second runner/source implementation.
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary))
            for cause in ("json", "shape", "workspace", "members", "prelude"):
                with self.subTest(cause=cause):
                    status, calls = self.execute(root, metadata_cause=cause)
                    self.assertEqual((status, len(calls)), (1, 1))
            status, calls = self.execute(root, output_cause="missing")
            self.assertEqual((status, len(calls)), (1, 2))
            status, calls = self.execute(root, fault="spawn")
            self.assertEqual((status, len(calls)), (1, 1))

    def test_index_working_and_untracked_mutations_bind_the_real_subject(self):
        # E6 actual Git index/WT/untracked axes: even HEAD->WT cancels to empty,
        # or the untracked path/status stays fixed, changed bytes reject before
        # test. The original shared working snapshot owns every axis/hash.
        for axis in ("index", "working", "untracked"):
            with self.subTest(axis=axis), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary))
                (root / "untracked.txt").write_text("before\n")
                status, calls = self.execute(root, mutation=axis)
                self.assertEqual((status, len(calls)), (1, 1))

    def test_unknown_source_before_start_admits_no_command(self):
        # E7 real nonrepository -> originalGit128, zero Cargo. A subdirectory of
        # a different admitted root is a ValueError refusal, also zero Cargo;
        # neither becomes an empty successful work plan or fake source witness.
        with tempfile.TemporaryDirectory() as temporary:
            outer = Path(temporary)
            empty = outer / "not-a-repository"
            empty.mkdir()
            status, calls = self.execute(empty)
            self.assertEqual((status, calls), (128, []))
            root = self.repository(outer)
            nested = root / "nested"
            nested.mkdir()
            status, calls = self.execute(nested)
            self.assertEqual((status, calls), (1, []))

    def test_foreign_hook_cannot_read_or_modify_the_decoy(self):
        # E8 foreign DIR/WT/index -> the existing Git environment authority
        # detaches to our real subject. Decoy HEAD/index/config/refs and WT bytes
        # remain exact; no fixture Git call may inherit the outer hook's repo.
        with tempfile.TemporaryDirectory() as temporary:
            outer = Path(temporary)
            root, decoy = self.repository(outer), self.repository(outer, "decoy")
            paths = [decoy / "tracked.txt", decoy / ".git/HEAD", decoy / ".git/index", decoy / ".git/config",
                     *sorted((decoy / ".git/refs").rglob("*"))]
            protected = {p: p.read_bytes() for p in paths if p.is_file()}
            before = formal_sources.source_facts(decoy)
            with mock.patch.dict(os.environ, GIT_DIR=str(decoy / ".git"),
                                 GIT_WORK_TREE=str(decoy), GIT_INDEX_FILE=str(decoy / ".git/index")):
                status, calls = self.execute(root)
            self.assertEqual((status, len(calls)), (0, 2))
            self.assertEqual(formal_sources.source_facts(decoy), before)
            self.assertEqual({p: p.read_bytes() for p in protected}, protected)


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SdkConsumerTests)
        if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
            return 1
        print("SDK consumer self-test: admission paths and exact task execution are enforced")
        return 0
    status = run_consumer()
    if status:
        return status
    print("SDK consumer: all seven task cases passed in an independent minimal-feature workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
