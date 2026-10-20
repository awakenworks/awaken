#!/usr/bin/env python3
"""Compile and execute the independent SDK consumer without workspace features."""
from __future__ import annotations

import argparse
import contextlib
import io
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tarfile
import tomllib
import unittest
from unittest import mock

import crate_test
import _sdk_archives
import _sdk_compiler_input
import gate_tools
import check_gate_freshness
from public_api_selection import selected_crates
import formal_sources
import rust_test_plan
import _sdk_semver_evidence as semver_evidence
from generate_hooks import gate_mode_source, glob_matches

# The independent manifest and its path dependencies are a different feature
# graph from the workspace. The existing Rust gate/generator owns their union.
INPUT_GLOBS = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**", "crates/**",
    "examples/sdk-consumer/**", "scripts/ci/*.py", "scripts/ci/check-rust.sh",
)

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "examples/sdk-consumer/Cargo.toml"
SDK_TARGET = "x86_64-unknown-linux-gnu"
EXPECTED = {
    "single_turn_exposes_identity_state_and_committed_output",
    "multiple_turns_share_one_store_and_thread",
    "one_typed_tool_definition_supplies_schema_and_execution",
    "streaming_delivers_progress_and_commits_complete_unicode_output",
    "cancelled_input_never_enters_the_model",
    "asynchronous_approval_resumes_the_committed_ticket",
    "filesystem_restart_reads_receipt_and_replays_terminal_without_a_model",
    "web_policy_configuration_remains_usable_through_sdk_reexports",
}


def consumer_calls(source: str) -> int:
    """Recognize the gate's direct and dependency-scoped executable forms."""
    direct = ["python3", "scripts/ci/check_sdk_consumer.py"]
    count = 0
    for line in re.sub(r"\\\r?\n", " ", source).splitlines():
        try:
            command = shlex.split(line, comments=True)
        except ValueError:
            continue  # An unrecognized call cannot admit the consumer graph.
        if command == direct:
            count += 1
        elif (len(command) == 6 and command[:2] == ["scoped_step", "sdk-consumer"]
              and command[3] == "$(consumed_packages examples/sdk-consumer/Cargo.toml)"
              and command[4:] == direct):
            count += 1
    return count


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
        if consumer_calls(source) != 1:
            errors.append(f"Rust gate {mode} must execute the independent SDK consumer exactly once")
    return errors


def validate_task_evidence(output: str) -> None:
    executed = re.findall(r"^test ([a-z_]+) \.\.\. ok$", output, re.MULTILINE)
    summary = f"{len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    if set(executed) != EXPECTED or len(executed) != len(EXPECTED) or summary not in output:
        raise RuntimeError(f"SDK consumer task evidence is incomplete: expected={sorted(EXPECTED)}, executed={sorted(executed)}")


def validate_semver_evidence(output: str, expected: list[str], major_audit=None) -> None:
    semver_evidence.validate_standard(output, expected, major_audit)


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
    # S3 causes: the actual sdk-consumer scope, original dependency subject,
    # and exact command split across two continuations -> one admitted call.
    # This is the same executable cause as the one-line scoped form below;
    # wrong scope/comment/extra arguments/duplicates remain refusal causes.
    wrapped = gate.replace(invocation, 'scoped_step "sdk-consumer" "fix" \\\n  "$(consumed_packages examples/sdk-consumer/Cargo.toml)" \\\n  ' + invocation)
    assert not admission_errors(owner, wrapped)
    assert admission_errors(owner.replace("examples/sdk-consumer/**,", ""), gate)
    for changed in (gate.replace(invocation, "true"), gate + invocation + '\n',
                    gate.replace(invocation, '# ' + invocation),
                    gate.replace(invocation, invocation + " --self-test"),
                    gate.replace(invocation, 'if [ "$mode" = "--push" ]; then\n' + invocation + '\nfi'),
                    gate.replace(invocation, 'if [ "$mode" = "--full" ]; then\n' + invocation + '\nfi')):
        assert admission_errors(owner, changed)
    scoped = ('scoped_step "sdk-consumer" "external SDK tasks" \\\n'
              '  "$(consumed_packages examples/sdk-consumer/Cargo.toml)" ' + invocation)
    scoped_gate = gate.replace(invocation, scoped)
    assert not admission_errors(owner, scoped_gate)
    for changed in (
        scoped_gate + scoped + '\n',
        scoped_gate.replace(scoped, '# ' + scoped.replace('\\\n', ' ')),
        scoped_gate.replace(invocation, invocation + ' --self-test'),
        scoped_gate.replace('examples/sdk-consumer/Cargo.toml', 'fuzz/Cargo.toml'),
        scoped_gate.replace('"sdk-consumer"', '"other-consumer"'),
        scoped_gate.replace(scoped, 'echo ' + scoped.replace('\\\n', ' ')),
    ):
        assert admission_errors(owner, changed), "scoped SDK admission must retain execution and its dependency graph"
    passing = "\n".join(f"test {name} ... ok" for name in sorted(EXPECTED))
    summary = f"\ntest result: ok. {len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
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


def sdk_source_observation():
    """Observe and render the original working source without choosing policy."""
    observed, facts = rust_test_plan.working_subject()
    source = {"revision": facts["revision"].decode().strip(),
              "tree": facts["tree"].decode().strip(),
              "index_tree": facts["index_tree"].decode().strip(),
              "working_subject": observed}
    return observed, facts, source


def capture_sdk_command(command, *, merged: bool, **arguments):
    """The SDK owner's one stdio adapter; command and status stay upstream."""
    program = Path(command[0]).name.casefold().removesuffix(".exe").removesuffix(".cmd")
    arguments_after_program = command[1:]
    if arguments_after_program and arguments_after_program[0].startswith("+"):
        arguments_after_program = arguments_after_program[1:]
    metadata = program == "cargo" and bool(arguments_after_program) and arguments_after_program[0] == "metadata"
    # Cargo progress and unused-patch warnings are diagnostics, even on success.
    # Metadata's stdout is the JSON protocol; keep it separate before parsing.
    merged = merged and not metadata
    stdio = {"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT} if merged else {"capture_output": True}
    return subprocess.run(command, text=True, **stdio, **arguments)


def retain_sdk_command_output(directory: Path, label: str, completed) -> None:
    """Retain the original channels and the existing combined diagnostic log."""
    stdout, stderr = completed.stdout or "", completed.stderr or ""
    for suffix, value in ((".stdout.log", stdout), (".stderr.log", stderr), (".log", stdout + stderr)):
        (directory / (label + suffix)).write_text(value)


def retain_sdk_result(directory: Path, document: dict) -> None:
    """Serialize the existing SDK receipt; its caller owns failure policy."""
    (directory / "commands.json").write_text(json.dumps(document, indent=2) + "\n")


def major_producer_packages(metadata: dict, sdk_root: Path, *, current: bool) -> dict:
    """Validate the existing Cargo member observation before rendering inputs."""
    if Path(metadata["workspace_root"]).resolve() != sdk_root.resolve():
        raise ValueError("SDK major producer metadata has a foreign root")
    packages = crate_test._members(metadata)
    for name in semver_evidence.consumer_package_names():
        package = packages[name]
        expected = (semver_evidence.COHORT[name][int(current)]
                    if name in semver_evidence.COHORT else package["version"])
        path = Path(package["manifest_path"]).resolve()
        if (package["version"] != expected or package.get("source") is not None
                or not path.is_relative_to(sdk_root.resolve()) or not path.is_file()
                or ((package.get("metadata") or {}).get("awaken") or {}).get("product") != "sdk"):
            raise ValueError("SDK major producer identity is outside its declared source/cohort")
        authored = tomllib.loads(path.read_text()).get("package", {})
        version = authored.get("version")
        if isinstance(version, dict):
            if set(version) != {"workspace"} or version["workspace"] is not True:
                raise ValueError("SDK major producer has invalid workspace version inheritance")
            version = tomllib.loads((sdk_root / "Cargo.toml").read_text()).get("workspace", {}).get("package", {}).get("version")
        if (authored.get("name") != name or version != expected
                or ((authored.get("metadata") or {}).get("awaken") or {}).get("product") != "sdk"):
            raise ValueError("SDK major producer metadata differs from its authored member identity")
    return packages


def validate_retained_major_receipt(receipt: dict, work: Path, source: dict, baseline: str,
                                    feature: str, target: str) -> None:
    """The existing SDK IO owner binds the pure receipt to retained evidence."""
    try:
        planned = semver_evidence.witness_steps(ROOT, work, baseline, feature, target)
        outputs = {}
        if len(receipt.get("steps", [])) != len(planned):
            raise ValueError("SDK major retained step evidence is incomplete")
        for row, step in zip(receipt["steps"], planned):
            name = step["name"]
            stdout = (work / (name + ".stdout.log")).read_bytes()
            stderr = (work / (name + ".stderr.log")).read_bytes()
            combined = (work / (name + ".log")).read_bytes()
            if combined != stdout + stderr or hashlib.sha256(combined).hexdigest() != row.get("log_sha256"):
                raise ValueError("SDK major retained output changed or is incomplete")
            outputs[name] = (stdout.decode(), stderr.decode())
        sides = {}
        for current, sdk_root in ((False, work / "baseline-source"), (True, ROOT)):
            side = "current" if current else "baseline"
            consumer = work / (side + "-consumer")
            packages = major_producer_packages(json.loads(outputs[side + "-sdk-metadata"][0]), sdk_root, current=current)
            facts = semver_evidence.validate_consumer_metadata(
                json.loads(outputs[side + "-consumer-metadata"][0]), consumer, packages, current=current, feature=feature)
            manifest = consumer / "Cargo.toml"
            if manifest.read_text() != semver_evidence.consumer_manifest(packages, current=current, feature=feature):
                raise ValueError("SDK major consumer manifest differs from the original dependency inputs")
            facts["manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
            facts["lock_sha256"] = hashlib.sha256((consumer / "Cargo.lock").read_bytes()).hexdigest()
            sides[side] = facts
            for name, (code, marker) in semver_evidence.witness_cases(current).items():
                text = semver_evidence.WITNESS_SOURCES[name]
                if (consumer / "cases" / (name + ".rs")).read_text() != text:
                    raise ValueError("SDK major downstream source changed")
                output = "".join(outputs[side + "-" + name])
                errors = semver_evidence.validate_compiler_case(output, 101 if code else 0, text,
                    "cases/" + name + ".rs", code, marker)
                recorded = [case for case in receipt["cases"] if case["side"] == side and case["case"] == name]
                if len(recorded) != 1 or recorded[0]["compiler_errors"] != errors:
                    raise ValueError("SDK major diagnostic differs from its retained original output")
        if hashlib.sha256((work / "baseline.tar").read_bytes()).hexdigest() != receipt.get("baseline_archive_sha256"):
            raise ValueError("SDK major baseline archive changed")
        semver_evidence.validate_witness_receipt(receipt, source, baseline, feature,
            target=target, root=ROOT, work=work, side_facts=sides)
    except (KeyError, TypeError, AttributeError) as error:
        raise ValueError(str(error)) from error


def run_major_consumer_audit(spec_path: Path) -> int:
    """One explicit SDK test action; compiler refusals are its asserted data."""
    values = crate_test.environment(ROOT)
    clean_environment = formal_sources.detached_git_environment()
    values = {**clean_environment, **{key: values[key] for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR")}}
    completed, journal = None, {"steps": [], "cases": [], "sides": {}}
    work = None
    try:
        spec = json.loads(spec_path.read_text())
        target = Path(values["CARGO_BUILD_BUILD_DIR"]).resolve() / "test-timings"
        if not spec_path.resolve().is_relative_to(target):
            raise ValueError("SDK major test specification is outside its owned qualification evidence")
        feature = spec["feature"]
        if (feature not in ("default", "all") or spec["target"] != SDK_TARGET
                or not re.fullmatch(r"[0-9a-f]{40}", spec["baseline_rev"])):
            raise ValueError("SDK major test specification has an unknown feature or baseline")
        journal.update(source=spec["source"], baseline_rev=spec["baseline_rev"], feature=feature, target=spec["target"])
        owned_subjects = {}

        def verify():
            _, facts, actual = sdk_source_observation()
            if actual != spec["source"] or facts["status"]:
                raise ValueError("SDK major consumer source changed or is not clean")
            for directory, (expected, ignore_lock) in owned_subjects.items():
                observed = _sdk_archives.file_hashes(directory)
                if ignore_lock:
                    observed.pop("Cargo.lock", None)
                if observed != expected:
                    raise ValueError("SDK major owned baseline/consumer source or lock changed")
        verify()
        work = spec_path.parent / (feature + "-major-consumers")
        work.mkdir()  # Fresh evidence only: never reuse an earlier baseline tree.
        planned = {step["name"]: step["command"] for step in semver_evidence.witness_steps(
            ROOT, work, spec["baseline_rev"], feature, spec["target"])}

        def capture(command, **arguments):
            nonlocal completed
            completed = None
            completed = capture_sdk_command(command, merged=True, **arguments)
            return completed

        def execute(name, *, directory=ROOT, validate=None):
            nonlocal completed
            completed = None
            def record(rows):
                row = rows[-1]
                row.update(log=str(work / (name + ".log")),
                    log_sha256=hashlib.sha256(((completed.stdout or "") + (completed.stderr or "")).encode()).hexdigest() if completed is not None else None)
                # Keep the sequence's one mutable observation, including source
                # and record faults; no second post-sequence step update/write.
                journal["steps"].append(row)
                try:
                    if completed is not None:
                        retain_sdk_command_output(work, name, completed)
                    if validate is not None and completed is not None:
                        try:
                            validate(completed.stdout, row)
                        except (KeyError, TypeError, AttributeError) as error:
                            raise ValueError(str(error)) from error
                finally:
                    retain_sdk_result(work, journal)
            return crate_test.execute_commands([planned[name]], values, root=directory,
                verify=verify, record=record, run=capture)

        archive = work / "baseline.tar"
        def archive_result(output, row):
            if row["exit_code"] == 0:
                journal["baseline_archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        status, _ = execute("baseline-archive", validate=archive_result)
        if status:
            return status
        baseline = work / "baseline-source"
        baseline.mkdir()
        if not hasattr(tarfile, "data_filter"):
            raise ValueError("SDK baseline extraction requires Python's standard tar data filter")
        with tarfile.open(archive, "r:") as source:
            source.extractall(baseline, filter="data")
        owned_subjects[baseline] = (_sdk_archives.file_hashes(baseline), False)
        for current, sdk_root in ((False, baseline), (True, ROOT)):
            side = "current" if current else "baseline"
            side_packages = {}
            def sdk_metadata(output, row):
                nonlocal side_packages
                data = json.loads(output)
                side_packages = major_producer_packages(data, sdk_root, current=current)
            status, _ = execute(side + "-sdk-metadata", validate=sdk_metadata)
            if status:
                return status
            consumer = work / (side + "-consumer")
            (consumer / "cases").mkdir(parents=True)
            manifest = consumer / "Cargo.toml"
            manifest.write_text(semver_evidence.consumer_manifest(side_packages, current=current, feature=feature))
            cases = semver_evidence.witness_cases(current)
            for name in cases:
                (consumer / "cases" / (name + ".rs")).write_text(semver_evidence.WITNESS_SOURCES[name])
            owned_subjects[consumer] = (_sdk_archives.file_hashes(consumer), True)
            def consumer_metadata(output, row):
                journal["sides"][side] = semver_evidence.validate_consumer_metadata(
                    json.loads(output), consumer, side_packages, current=current, feature=feature)
                journal["sides"][side]["manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
                journal["sides"][side]["lock_sha256"] = hashlib.sha256((consumer / "Cargo.lock").read_bytes()).hexdigest()
                owned_subjects[consumer] = (_sdk_archives.file_hashes(consumer), False)
            status, _ = execute(side + "-consumer-metadata", directory=consumer, validate=consumer_metadata)
            if status:
                return status
            for name, (expected_code, marker) in cases.items():
                source = semver_evidence.WITNESS_SOURCES[name]
                accepted = False
                def compiler_result(output, row):
                    nonlocal accepted
                    errors = semver_evidence.validate_compiler_case(output, row["exit_code"], source,
                        "cases/" + name + ".rs", expected_code, marker)
                    journal["cases"].append({"side": side, "case": name, "original_exit_code": row["exit_code"],
                        "expected_code": expected_code, "compiler_errors": errors,
                        "source_sha256": hashlib.sha256(source.encode()).hexdigest()})
                    accepted = True
                status, rows = execute(side + "-" + name, directory=consumer, validate=compiler_result)
                # The generic owner still returns original101. Only this
                # explicit negative test may assert its exact counterexample.
                if status and not (status == 101 and expected_code and accepted
                                   and rows and "subject_error" not in rows[-1]
                                   and "record_error" not in rows[-1]):
                    return status
        verify()
        journal["exit_code"] = 0
        validate_retained_major_receipt(journal, work, spec["source"], spec["baseline_rev"], feature, spec["target"])
        retain_sdk_result(work, journal)
        return 0
    except subprocess.CalledProcessError as error:
        return error.returncode if error.returncode >= 0 else 128 - error.returncode
    except (OSError, ValueError, KeyError, TypeError, AttributeError, tarfile.TarError) as error:
        print(f"SDK major consumer audit refused: {error}", file=sys.stderr)
        return 1


def run_consumer(*, qualification: str = "", baseline_rev: str = "") -> int:
    """Keep SDK admission policy; the existing sequence owns source and status."""
    values = crate_test.environment(ROOT)
    commands = [
        ["cargo", "metadata", "--manifest-path", str(MANIFEST), "--no-deps", "--format-version", "1", "--locked"],
        ["cargo", "test", "--manifest-path", str(MANIFEST), "--locked", "--test", "tasks", "--", "--test-threads=1"],
    ]
    subject = None
    completed = None
    evidence = None
    members = {}
    unpacked = {}
    archive_subject = None
    consumer_subject = None
    consumer_lock = None
    major_commands = {}
    document = {"qualification": qualification, "target": SDK_TARGET,
                "baseline_kind": "first pre-release source contract" if baseline_rev else None,
                "baseline_rev": baseline_rev or None}

    def verify() -> None:
        nonlocal subject
        observed, facts, source = sdk_source_observation()
        if qualification and (facts["status"] or facts["staged"] or facts["working"] or facts["untracked"]):
            raise ValueError("SDK qualification requires an exact clean source/index/worktree")
        if subject is None:
            subject = observed
            if qualification:
                document["source"] = source
        elif observed != subject:
            raise ValueError("SDK consumer source changed before or during execution")
        if archive_subject is not None:
            if _sdk_archives.file_hashes(evidence / "unpacked") != archive_subject:
                raise ValueError("unpacked package source changed during artifact consumption")
            for artifact in document["artifacts"]["default"]:
                if hashlib.sha256(Path(artifact["path"]).read_bytes()).hexdigest() != artifact["sha256"]:
                    raise ValueError("package archive changed during artifact consumption")
            files = _sdk_archives.file_hashes(evidence / "consumer")
            files.pop("Cargo.lock", None)
            if files != consumer_subject:
                raise ValueError("copied consumer source changed during artifact consumption")
            if consumer_lock is not None and hashlib.sha256(
                    (evidence / "consumer/Cargo.lock").read_bytes()).hexdigest() != consumer_lock:
                raise ValueError("artifact consumer lock changed after resolution")

    def capture(command, **arguments):
        # Only preserve the existing stdio contract. No retry, deadline, process
        # manager or command loop: Cargo admission and lifecycle stay upstream.
        nonlocal completed
        completed = None
        completed = capture_sdk_command(command, merged=bool(qualification), **arguments)
        if qualification and completed.stderr is None:
            completed.stderr = ""
        return completed

    def assess_result(row):
        action = major_commands.get(tuple(row["command"]))
        if action is None or action[0] != "audit":
            return None
        feature = action[1]
        witness = document.get("major_consumer_audits", {}).get(feature)
        validate_retained_major_receipt(witness or {}, evidence / (feature + "-major-consumers"),
                                       document["source"], baseline_rev, feature, document["target"])
        observed = semver_evidence.validate_audit(completed.stdout + completed.stderr, row["exit_code"],
                                                 approved=semver_evidence.APPROVED_FINDINGS)
        document.setdefault("major_audits", {})[feature] = {
            "source": document["source"], "baseline_rev": baseline_rev,
            "feature": feature, "packages": observed, "consumer_audit": witness,
        }
        return "accepted_major" if row["exit_code"] == 1 else None

    def record(rows) -> None:
        nonlocal unpacked, archive_subject, consumer_subject, consumer_lock
        if completed is None:
            return
        if rows[-1]["command"][1] == "test":
            sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        if qualification == "artifact":
            phase = ("package", "metadata", "tasks", "sample")[len(rows) - 1]
            retain_sdk_command_output(evidence, phase, completed)
            try:
                if rows[-1]["exit_code"] == 0:
                    if phase == "package":
                        archive_directory = Path(values["CARGO_BUILD_BUILD_DIR"])
                        if not archive_directory.is_absolute():
                            archive_directory = ROOT / archive_directory
                        artifacts = _sdk_archives.capture_archives(
                            archive_directory / "package", evidence / "default-artifacts",
                            document["artifact_packages"], members, document["source"]["revision"], ROOT)
                        document["artifacts"] = {"default": artifacts}
                        unpacked = _sdk_archives.unpack_archives(artifacts, members, evidence / "unpacked")
                        _sdk_archives.stage_consumer(MANIFEST, evidence / "consumer", unpacked, members)
                        archive_subject = _sdk_archives.file_hashes(evidence / "unpacked")
                        consumer_subject = _sdk_archives.file_hashes(evidence / "consumer")
                        document["archive_source_hashes"] = archive_subject
                        document["consumer_source_hashes"] = consumer_subject
                    elif phase == "metadata":
                        metadata = json.loads(completed.stdout)
                        document["consumed_packages"] = _sdk_archives.validate_consumer_graph(
                            metadata, evidence / "consumer", unpacked, members)
                        (evidence / "consumer-metadata.json").write_text(completed.stdout)
                        consumer_lock = hashlib.sha256((evidence / "consumer/Cargo.lock").read_bytes()).hexdigest()
                        document["consumer_lock_sha256"] = consumer_lock
                    elif phase == "tasks":
                        validate_task_evidence(completed.stdout)
                        document["executed_tasks"] = sorted(EXPECTED)
                    elif "Hello from the SDK" not in completed.stdout:
                        raise ValueError("artifact sample did not produce the original SDK response")
            except (KeyError, TypeError, RuntimeError, tarfile.TarError) as error:
                raise ValueError(str(error)) from error
            document["commands"] = rows
            retain_sdk_result(evidence, document)
            return
        if qualification:
            command = rows[-1]["command"]
            action = major_commands.get(tuple(command))
            feature = action[1] if action else "all" if "--all-features" in command else "default"
            label = feature + ("-major-" + action[0] if action else "")
            retain_sdk_command_output(evidence, label, completed)
            if rows[-1]["exit_code"] == 0 and action and action[0] == "consumers":
                receipt = json.loads((evidence / (feature + "-major-consumers/commands.json")).read_text())
                validate_retained_major_receipt(receipt, evidence / (feature + "-major-consumers"),
                                               document["source"], baseline_rev, feature, document["target"])
                document.setdefault("major_consumer_audits", {})[feature] = receipt
            if rows[-1]["exit_code"] == 0 and qualification == "semver" and action is None:
                audit = document.get("major_audits", {}).get(feature)
                if major_commands and audit is None:
                    raise ValueError("SDK major observation has no same-feature audited changes")
                validate_semver_evidence(completed.stdout + completed.stderr, document["sdk_crates"],
                                         audit["packages"] if audit else None)
            if rows[-1]["exit_code"] == 0 and qualification == "package":
                archive_directory = Path(values["CARGO_BUILD_BUILD_DIR"])
                if not archive_directory.is_absolute():
                    archive_directory = ROOT / archive_directory
                document.setdefault("artifacts", {})[feature] = _sdk_archives.capture_archives(
                    archive_directory / "package", evidence / (feature + "-artifacts"),
                    document["sdk_crates"], members, document["source"]["revision"], ROOT)
            document["commands"] = rows
            retain_sdk_result(evidence, document)
            return
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

    if qualification:
        failure = 0
        try:
            verify()
            if qualification == "semver":
                if not re.fullmatch(r"[0-9a-f]{40}", baseline_rev) or baseline_rev == document["source"]["revision"]:
                    raise ValueError("SDK SemVer requires an explicit full non-HEAD pre-release source baseline")
                for arguments in (("cat-file", "-e", baseline_rev + "^{commit}"),
                                  ("merge-base", "--is-ancestor", baseline_rev, "HEAD")):
                    result = check_gate_freshness.git(ROOT, *arguments)
                    if result.returncode:
                        raise ValueError("SDK SemVer baseline must exist and be an ancestor of this source")
                tool = gate_tools.find_tool(gate_tools.load(), "cargo-semver-checks")
                if tool is None:
                    raise ValueError("SDK SemVer tool is not declared in gate_tools.json")
                pin = tool.get("version") or {}
                if not pin.get("command") or not any(field in pin for field in ("expect", "contains")):
                    raise ValueError("SDK SemVer tool has no declared version observation")
                found = gate_tools.locate(tool)
                if not found.usable:
                    raise ValueError(f"SDK SemVer tool is {found.status}: {tool['name']}")
                problem = gate_tools.version_problem(tool, found)
                if problem:
                    raise ValueError(problem)
            metadata = crate_test.full_graph_metadata([], root=ROOT, environment=values, host_only=False)
            if Path(metadata["workspace_root"]).resolve() != ROOT.resolve():
                raise ValueError("SDK qualification metadata is not this source workspace")
            members = crate_test._members(metadata)
            _, sdk = selected_crates(list(members.values()))
            if not sdk:
                raise ValueError("SDK qualification has no publishable SDK members")
            document["sdk_crates"] = sdk
            if qualification in ("package", "semver"):
                document["compiler_inputs"] = _sdk_compiler_input.qualification(
                    ROOT, document["source"]["revision"], members, sdk, metadata, values, baseline_rev)
                values.update(document["compiler_inputs"]["environment"])
            target = Path(values["CARGO_BUILD_BUILD_DIR"])
            if not target.is_absolute():
                target = ROOT / target
            target /= "test-timings"
            target.mkdir(parents=True, exist_ok=True)
            evidence = Path(tempfile.mkdtemp(prefix="sdk-qualification-", dir=target))
            (evidence / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
            selectors = [word for name in sdk for word in ("-p", name)]
            if qualification in ("package", "artifact"):
                command = ["cargo", "+1.96.0", "package", "--locked", "--target", document["target"]]
                if qualification == "package":
                    commands = [command + selectors, command + ["--all-features"] + selectors]
                else:
                    names = _sdk_archives.consumer_packages(MANIFEST, members, sdk)
                    document["artifact_packages"] = names
                    manifest = str(evidence / "consumer/Cargo.toml")
                    commands = [
                        command + [word for name in names for word in ("-p", name)],
                        ["cargo", "metadata", "--manifest-path", manifest, "--format-version", "1",
                         "--filter-platform", document["target"]],
                        ["cargo", "test", "--manifest-path", manifest, "--locked", "--target", document["target"],
                         "--test", "tasks", "--", "--test-threads=1"],
                        ["cargo", "run", "--manifest-path", manifest, "--locked", "--target", document["target"]],
                    ]
            else:
                command = ["cargo", "semver-checks", "check-release", "--baseline-rev", baseline_rev,
                           "--target", document["target"]]
                commands = [command + [features] + selectors
                            for features in ("--default-features", "--all-features")]
                if any(name in sdk and members[name]["version"] not in versions
                       for name, versions in semver_evidence.COHORT.items()):
                    raise ValueError("SDK declared major cohort has an unknown source version")
                majors = sorted(name for name, versions in semver_evidence.COHORT.items()
                                if name in sdk and members[name]["version"] == versions[1])
                if majors:
                    if set(majors) != set(semver_evidence.COHORT):
                        raise ValueError("SDK declared major cohort is incomplete")
                    original = commands
                    commands = []
                    major_selectors = [word for name in majors for word in ("-p", name)]
                    for standard in original:
                        feature = "all" if "--all-features" in standard else "default"
                        spec_path = evidence / (feature + "-major-spec.json")
                        spec_path.write_text(json.dumps({key: document[key] for key in ("source", "baseline_rev", "target")}
                            | {"feature": feature}, indent=2) + "\n")
                        witness = [sys.executable, str(Path(__file__).resolve()), "--major-consumer-audit", str(spec_path)]
                        audit = command + ["--all-features" if feature == "all" else "--default-features",
                                           "--release-type", "patch"] + major_selectors
                        major_commands[tuple(witness)] = ("consumers", feature)
                        major_commands[tuple(audit)] = ("audit", feature)
                        commands.extend((witness, audit, standard))
        except subprocess.CalledProcessError as error:
            failure = error.returncode if error.returncode >= 0 else 128 - error.returncode
            print(f"SDK qualification metadata/source failed: {error}", file=sys.stderr)
        except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
            failure = 1
            print(f"SDK qualification refused: {error}", file=sys.stderr)
        # Even a failed preflight checks source after IO. The original failure
        # outranks this secondary fault, matching the sole sequence's policy.
        try:
            verify()
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            failure = failure or 1
            print(f"SDK qualification source verification failed: {error}", file=sys.stderr)
        if failure:
            return failure
        print(f"SDK qualification evidence: {evidence}")

    status, rows = crate_test.execute_commands(commands, values,
                                           root=ROOT, verify=verify, record=record, run=capture,
                                           assess_result=assess_result if major_commands else None)
    if qualification:
        document["commands"] = rows
        document["exit_code"] = status
        try:
            retain_sdk_result(evidence, document)
        except OSError as error:
            print(f"Cannot retain SDK qualification result: {error}", file=sys.stderr)
            return status or 1
    return status


class SdkConsumerTests(unittest.TestCase):
    """Private Git subjects; Cargo calls below are controlled at the sole IO edge."""

    @staticmethod
    def git(root: Path, *arguments: str) -> None:
        subprocess.run(["git", "-C", str(root), *arguments], check=True,
                       env=formal_sources.detached_git_environment(), capture_output=True)

    @staticmethod
    def compiler_source(root: Path, *, sdk_k8s=False, model="v1_30", version="0.25.0"):
        """Private Git facts for the existing controlled qualifier, never Cargo."""
        feature = model if isinstance(model, list) else [model]
        declaration = ('k8s-openapi={version=' + json.dumps(version) + ',features='
                       + json.dumps(feature) + '}\n') if model is not None else ''
        (root / "Cargo.toml").write_text('[workspace]\nmembers=["crates/*"]\n'
            '[workspace.dependencies]\n' + declaration)
        for name, product, dependencies in (
            ("fixture-contract", "sdk", ''),
            ("fixture-provider", "sdk", 'genai="0.0.0"\n' + ('kube={version="4.2.0",optional=true}\n' if sdk_k8s else '')),
            ("fixture-internal", "platform", 'k8s-openapi={version=' + json.dumps(version)
                + ',features=[' + json.dumps("v1_30" if version == "0.25.0" else "v1_32") + ']}\n'),
        ):
            path = root / "crates" / name / "Cargo.toml"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('[package]\nname=' + json.dumps(name) + '\nversion="1.0.0"\n'
                '[package.metadata.awaken]\nproduct=' + json.dumps(product) + '\napi_snapshot=true\n'
                '[dependencies]\n' + dependencies)
        packages = [
            ("fixture-contract", "1.0.0", None, []),
            ("fixture-provider", "1.0.0", None, ["genai", *(["kube"] if sdk_k8s else [])]),
            ("fixture-internal", "1.0.0", None, ["k8s-openapi"]),
            ("genai", "0.0.0", "git+fixture", []),
            ("kube", "4.2.0", "registry+https://github.com/rust-lang/crates.io-index", ["k8s-openapi"]),
            ("k8s-openapi", version, "registry+https://github.com/rust-lang/crates.io-index", []),
        ]
        lock = "version=4\n"
        for name, version, source, dependencies in packages:
            lock += '\n[[package]]\nname=' + json.dumps(name) + '\nversion=' + json.dumps(version) + '\n'
            if source:
                lock += 'source=' + json.dumps(source) + '\n'
            lock += 'dependencies=' + json.dumps(dependencies) + '\n'
        (root / "Cargo.lock").write_text(lock)

    def repository(self, outer: Path, name: str = "subject", *, successor: bool = False) -> Path:
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
        (root / ".gitignore").write_text("target/\n")
        (root / "tracked.txt").write_text("before\n")
        self.compiler_source(root)
        self.git(root, "init", "--quiet")
        self.git(root, "config", "user.name", "SDK source fixture")
        self.git(root, "config", "user.email", "fixture@example.invalid")
        self.git(root, "config", "commit.gpgsign", "false")
        self.git(root, "config", "core.hooksPath", str(outer / "fixture-hooks"))
        self.git(root, "add", ".")
        self.git(root, "commit", "--quiet", "-m", "private source")
        if successor:
            (root / "tracked.txt").write_text("current source\n")
            self.git(root, "add", "tracked.txt")
            self.git(root, "commit", "--quiet", "-m", "private successor")
        return root

    def execute(self, root: Path, *, metadata_status=0, test_status=0,
                mutation="", fault="", metadata_cause="", output_cause="", phase="metadata",
                qualification="", baseline_rev=None, tool_cause="",
                artifact_metadata_status=None, compiler_cause="", compiler_observations=None,
                active_major=False, major_action_run=None, major_failure=None) -> tuple[int, list]:
        manifest = root / "examples/sdk-consumer/Cargo.toml"
        metadata = {"workspace_root": str(manifest.parent), "workspace_members": ["sdk"],
                    "packages": [{"dependencies": []}]}
        if qualification:
            packages = [
                {"id": "sdk-contract", "name": "fixture-contract", "version": "1.0.0",
                 "publish": None, "dependencies": [],
                 "metadata": {"awaken": {"product": "sdk", "api_snapshot": True}}},
                {"id": "sdk-provider", "name": "fixture-provider", "version": "1.0.0",
                 "publish": None, "dependencies": [{"name": "genai", "source": "git+fixture"}],
                 "metadata": {"awaken": {"product": "sdk", "api_snapshot": True}}},
                {"id": "internal", "name": "fixture-internal", "version": "1.0.0",
                 "publish": None, "dependencies": [],
                 "metadata": {"awaken": {"product": "platform", "api_snapshot": True}}},
                {"id": "example", "name": "fixture-example", "version": "1.0.0",
                 "publish": [], "dependencies": [],
                 "metadata": {"awaken": {"product": "sdk", "api_snapshot": True}}},
            ]
            if active_major:
                # The active-major tests use this same IO fixture and the real
                # cohort names, rather than a second qualifier or command loop.
                # The remaining SDK members are selected, never omitted from
                # either original standard command.
                packages = [{"id": name, "name": name,
                    "version": semver_evidence.COHORT.get(name, ("1.0.0", "2.0.0"))[1]
                        if name in semver_evidence.COHORT else "1.0.0",
                    "publish": None, "dependencies": [], "features": {}, "source": None,
                    "metadata": {"awaken": {"product": "sdk", "api_snapshot": True}}}
                    for name in [*semver_evidence.consumer_package_names(),
                                 *("fixture-sdk-" + str(number).zfill(2) for number in range(
                                     22 - len(semver_evidence.consumer_package_names())))]]
                if metadata_cause == "major-partial":
                    packages[0]["version"] = "1.0.0"
                elif metadata_cause == "major-unknown":
                    for package in packages[:2]:
                        package["version"] = "3.0.0"
            metadata = {"workspace_root": str(root), "workspace_members": [p["id"] for p in packages],
                        "packages": packages,
                        "resolve": {"nodes": [{"id": p["id"], "deps": []} for p in packages]}}
            if compiler_cause:
                registry = "registry+https://github.com/rust-lang/crates.io-index"
                identity = registry + "#k8s-openapi@0.28.0"
                package = {"id": identity, "name": "k8s-openapi", "version": "0.28.0", "source": registry,
                           "features": {"std": [], "v1_32": [], "v1_33": []}}
                metadata["packages"].append(package)
                metadata["resolve"]["nodes"].append({"id": identity, "deps": []})
                if compiler_cause == "unsupported":
                    package["features"].pop("v1_32")
                elif compiler_cause == "identity":
                    package["source"] = "git+foreign"
                elif compiler_cause == "version":
                    package["version"] = "0.29.0"
                elif compiler_cause == "duplicate":
                    metadata["packages"].append({**package, "id": identity + "-other"})
                    metadata["resolve"]["nodes"].append({"id": identity + "-other", "deps": []})
                elif compiler_cause == "incomplete":
                    metadata["resolve"]["nodes"].pop()
            if baseline_rev is None:
                baseline_rev = subprocess.check_output(["git", "rev-parse", "HEAD^"], cwd=root,
                    env=formal_sources.detached_git_environment(), text=True).strip() if qualification == "semver" else ""
            revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                env=formal_sources.detached_git_environment(), text=True).strip()
            for package in packages:
                package["manifest_path"] = str(root / "crates" / package["name"] / "Cargo.toml")
            if qualification == "artifact":
                manifest.write_text('[package]\nname="sdk"\nversion="0.0.0"\n[workspace]\n'
                                    '[dependencies]\nfixture-contract={path="../../crates/fixture-contract"}\n'
                                    '[dev-dependencies]\nfixture-internal={path="../../crates/fixture-internal"}\n')
                for relative in ("src/lib.rs", "src/main.rs", "tests/tasks.rs"):
                    source = manifest.parent / relative
                    source.parent.mkdir(parents=True, exist_ok=True)
                    source.write_text("// original consumer fixture bytes; no Cargo is executed by this IO test\n")
                self.git(root, "add", ".")
                self.git(root, "commit", "--quiet", "-m", "private artifact input")
                revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                    env=formal_sources.detached_git_environment(), text=True).strip()
            if output_cause != "archive":
                # Test-owned archive data exercises only the record callback;
                # the IO mock below never implements Cargo or grants real package
                # qualification. Real Source/artifact receipts remain Root's.
                archive_root = root / "target/package"
                archive_root.mkdir(parents=True, exist_ok=True)
                for package in packages[:3] if qualification == "artifact" else packages[:2]:
                    filename = package["name"] + "-" + package["version"]
                    with tarfile.open(archive_root / (filename + ".crate"), "w:gz") as archive:
                        for member in ("Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"):
                            if output_cause == "member" and member == "Cargo.lock":
                                continue
                            content = b"private archive fixture\n"
                            if member == ".cargo_vcs_info.json":
                                vcs = {"git": {"sha1": revision}, "path_in_vcs": "crates/" + package["name"]}
                                if output_cause == "vcs-sha":
                                    vcs["git"]["sha1"] = "f" * 40
                                elif output_cause == "vcs-dirty":
                                    vcs["git"]["dirty"] = True
                                elif output_cause == "vcs-path":
                                    vcs["path_in_vcs"] = "crates/other-package"
                                elif output_cause == "vcs-malformed":
                                    vcs = {"git": None}
                                content = json.dumps(vcs).encode()
                            item = tarfile.TarInfo(filename + "/" + member)
                            item.size = len(content)
                            archive.addfile(item, io.BytesIO(content))
                    if output_cause == "corrupt":
                        (archive_root / (filename + ".crate")).write_bytes(b"invalid archive fixture\n")
            if metadata_cause == "empty":
                for package in packages:
                    package["metadata"]["awaken"]["product"] = "platform"
            elif metadata_cause == "selection":
                packages[0]["metadata"]["awaken"]["api_snapshot"] = False
        if metadata_cause == "workspace":
            metadata["workspace_root"] = str(root.parent) if qualification else str(root)
        elif metadata_cause == "members":
            metadata["workspace_members"] = []
        elif metadata_cause == "prelude":
            metadata["packages"][0]["dependencies"] = [{"name": "awaken-runtime-examples"}]
        elif metadata_cause == "shape":
            metadata = {}
        passing = "\n".join(f"test {name} ... ok" for name in sorted(EXPECTED))
        output = passing + f"\ntest result: ok. {len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        if qualification == "semver":
            # Pinned tool output is fixture data for the record validator;
            # it is not an implementation of Cargo or a real SemVer receipt.
            executed = ["fixture-contract", "fixture-provider"]
            if output_cause == "semver-empty":
                executed = []
            elif output_cause == "semver-missing":
                executed = executed[:1]
            elif output_cause == "semver-duplicate":
                executed = [executed[0], executed[0]]
            output = "\n".join(
                f"    Checking {name} v1.0.0 -> v1.0.0 (no change; assume patch)\n"
                "     Checked [ 0.003s] 222 checks: 222 pass, 30 skip\n"
                "     Summary no semver update required\n"
                f"    Finished [ 1.000s] {name}" for name in executed)
            if output_cause == "semver-unchecked":
                output = re.sub(r"^.*Checked.*\n", "", output, flags=re.MULTILINE)
            elif output_cause == "semver-zero-checks":
                output = output.replace("222 checks: 222 pass", "0 checks: 0 pass")
            if active_major:
                from sdk_semver_tests import SdkSemverEvidenceTests
                output = "".join(SdkSemverEvidenceTests.report(p["name"],
                    checks=0 if p["name"] in semver_evidence.COHORT else 222,
                    major=p["name"] in semver_evidence.COHORT) for p in packages)
                if output_cause == "semver-missing":
                    output = output[:output.rfind("Checking ")]
                elif output_cause == "semver-duplicate":
                    output += SdkSemverEvidenceTests.report(packages[0]["name"], checks=0, major=True)
                elif output_cause == "semver-zero-checks":
                    output = output.replace("222 checks: 222 pass", "0 checks: 0 pass")
        if output_cause == "missing":
            output = output.split("\n", 1)[1]
        calls = []
        actual_run = subprocess.run
        tool_manifest = gate_tools.load()
        if tool_cause == "undeclared":
            tool_manifest["tools"] = [tool for tool in tool_manifest["tools"] if tool["name"] != "cargo-semver-checks"]
        elif tool_cause == "pin":
            next(tool for tool in tool_manifest["tools"] if tool["name"] == "cargo-semver-checks").pop("version")

        def run(command, **arguments):
            program = Path(command[0]).name.casefold().removesuffix(".exe")
            if active_major and "--major-consumer-audit" in command:
                calls.append(list(command))
                if major_action_run is None:
                    raise AssertionError("active-major controlled action must use the original action fixture")
                return major_action_run(command, **arguments)
            if program not in ("cargo", "cargo-semver-checks"):
                return actual_run(command, **arguments)
            if program == "cargo":
                self.assertEqual(arguments["cwd"], root)
                self.assertEqual(arguments["env"]["CARGO_BUILD_BUILD_DIR"], arguments["env"]["CARGO_TARGET_DIR"])
            else:
                self.assertEqual(command[1:], ["semver-checks", "--version"])
            current_phase = ("package" if command[1] == "+1.96.0" else
                             "tool" if program == "cargo-semver-checks" else command[1])
            if compiler_observations is not None and current_phase in ("package", "semver-checks"):
                compiler_observations.append({"command": list(command),
                    "compiler_input": arguments["env"].get("K8S_OPENAPI_ENABLED_VERSION"),
                    "ambient": os.environ.get("K8S_OPENAPI_ENABLED_VERSION")})
            calls.append([program, *command[1:]])
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
            if qualification == "artifact" and current_phase not in ("metadata", phase):
                status = 0
            stdout = ("broken-json" if metadata_cause == "json" else json.dumps(metadata)) if current_phase == "metadata" else output
            if current_phase == "tool":
                status = 23 if tool_cause == "query" else 0
                stdout = "cargo-semver-checks 0.46.0\n" if tool_cause == "version" else "cargo-semver-checks 0.47.0\n"
            if active_major and current_phase == "semver-checks":
                from sdk_semver_tests import SdkSemverEvidenceTests
                feature = "all" if "--all-features" in command else "default"
                stage = "audit" if "--release-type" in command else "standard"
                if stage == "audit":
                    _, stdout = SdkSemverEvidenceTests().fixture()
                    status = 1
                    if output_cause == "audit-unknown":
                        stdout = stdout.replace("fixture::removed", "fixture::unreviewed")
                    elif output_cause == "audit-fatal":
                        stdout += "error: internal/tool failure\n"
                if major_failure and (feature, stage) == (major_failure["feature"], major_failure["stage"]):
                    status = major_failure["raw"]
                    if major_failure.get("source"):
                        (root / "tracked.txt").write_text("changed active-major source\n")
            if qualification == "artifact" and "--manifest-path" in command:
                consumer = Path(command[command.index("--manifest-path") + 1]).parent
                if current_phase == "metadata":
                    (consumer / "Cargo.lock").write_text("# private resolved lock fixture\n")
                    reached = [{"id": "consumer", "name": "sdk", "version": "0.0.0",
                                "manifest_path": str(consumer / "Cargo.toml"), "source": None}]
                    for package in packages[:3]:
                        reached.append({**package, "source": None, "manifest_path": str(
                            consumer.parent / "unpacked" / (package["name"] + "-1.0.0") / "Cargo.toml")})
                    if output_cause == "artifact-checkout":
                        reached[1]["manifest_path"] = packages[0]["manifest_path"]
                    elif output_cause == "artifact-registry":
                        reached[1]["source"] = "registry+fixture"
                    elif output_cause == "artifact-version":
                        reached[1]["version"] = "2.0.0"
                    elif output_cause == "artifact-duplicate":
                        reached.append(dict(reached[1]))
                    graph = {"workspace_root": str(consumer), "workspace_members": ["consumer"],
                             "packages": reached, "resolve": {"nodes": [
                                 {"id": p["id"], "deps": [{"pkg": d["id"]} for d in reached[1:]]
                                  if p["id"] == "consumer" else []} for p in reached]}}
                    if output_cause == "artifact-unreached":
                        graph["resolve"]["nodes"][0]["deps"] = [{"pkg": reached[2]["id"]}]
                    elif output_cause == "artifact-shape":
                        graph.pop("resolve")
                    stdout = json.dumps(graph)
                elif current_phase == "run":
                    stdout = "Run fixture: completed\nHello from the SDK\n"
                if output_cause in ("artifact-source", "artifact-consumer", "artifact-lock") and current_phase == phase:
                    target = (consumer.parent / "unpacked/fixture-contract-1.0.0/Cargo.toml"
                              if output_cause == "artifact-source" else consumer / (
                                  "src/lib.rs" if output_cause == "artifact-consumer" else "Cargo.lock"))
                    target.write_text("changed artifact input\n")
            stderr = "SDK captured diagnostic\n" if current_phase == phase else ""
            if (qualification == "artifact" and current_phase == "metadata"
                    and "--manifest-path" in command
                    and metadata_cause in ("warnings", "warnings-json")):
                # A7 models the real Cargo protocol at this existing IO edge:
                # metadata stdout is JSON; index/unused-patch messages are stderr.
                stderr = "    Updating crates.io index\nwarning: patch fixture was not used\n"
                if metadata_cause == "warnings-json":
                    stdout = "broken-json"
                if artifact_metadata_status is not None:
                    status = artifact_metadata_status
                if arguments.get("stderr") == subprocess.STDOUT:
                    stdout, stderr = stderr + stdout, None
            if status and arguments.get("check"):
                raise subprocess.CalledProcessError(status, command, output=stdout, stderr=stderr)
            return subprocess.CompletedProcess(command, status, stdout, stderr)

        class Output(io.StringIO):
            def write(self, value):
                if fault == "output" and value.startswith("SDK captured"):
                    raise OSError("exact SDK evidence output refusal")
                return super().write(value)

        with mock.patch.object(sys.modules[__name__], "ROOT", root), \
                mock.patch.object(sys.modules[__name__], "MANIFEST", manifest), \
                mock.patch.object(rust_test_plan, "ROOT", root), \
                mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(root / "target"),
                                CARGO_BUILD_BUILD_DIR=str(root / "target")), \
                mock.patch.object(subprocess, "run", side_effect=run), \
                mock.patch.object(gate_tools, "load", return_value=tool_manifest), \
                mock.patch.object(gate_tools, "locate", return_value=gate_tools.Found(
                    tool_cause if tool_cause in ("absent", "undecidable") else "present")), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(Output()):
            return run_consumer(qualification=qualification, baseline_rev=baseline_rev or ""), calls

    def test_original_argv_and_environment_reach_both_commands(self):
        # E1 valid independent metadata + all declared tasks -> 0. Retain the exact
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

    def test_cli_modes_keep_light_rules_separate_from_the_controlled_suite(self):
        # Q7 default/package/semver choose one original command owner after the
        # lightweight admission check; none runs the unittest suite. Invalid
        # mode combinations or missing/misplaced baseline fail2 before any
        # command or suite. Only explicit --self-test selects the existing suite.
        for arguments, expected in (
            ([], {"qualification": "", "baseline_rev": ""}),
            (["--qualify-package"], {"qualification": "package", "baseline_rev": ""}),
            (["--qualify-artifact"], {"qualification": "artifact", "baseline_rev": ""}),
            (["--qualify-semver", "--baseline-rev", "a" * 40],
             {"qualification": "semver", "baseline_rev": "a" * 40}),
        ):
            with self.subTest(arguments=arguments), mock.patch.object(sys, "argv", ["sdk", *arguments]), \
                    mock.patch.object(sys.modules[__name__], "run_consumer", return_value=0) as commands, \
                    mock.patch.object(unittest, "TextTestRunner") as suite, contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main(), 0)
                commands.assert_called_once_with(**expected)
                suite.assert_not_called()
        for arguments in (["--qualify-semver"], ["--baseline-rev", "a" * 40],
                          ["--qualify-package", "--baseline-rev", "a" * 40],
                          ["--qualify-package", "--qualify-semver"],
                          ["--self-test", "--qualify-package"], ["--qualify-packag"],
                          ["--qualify-artifact", "--qualify-package"],
                          ["--qualify-artifact", "--baseline-rev", "a" * 40]):
            with self.subTest(arguments=arguments), mock.patch.object(sys, "argv", ["sdk", *arguments]), \
                    mock.patch.object(sys.modules[__name__], "run_consumer") as commands, \
                    mock.patch.object(unittest, "TextTestRunner") as suite, contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as refusal:
                    main()
                self.assertEqual(refusal.exception.code, 2)
                commands.assert_not_called()
                suite.assert_not_called()
        with mock.patch.object(sys, "argv", ["sdk", "--self-test"]), \
                mock.patch.object(sys.modules[__name__], "run_consumer") as commands, \
                mock.patch.object(unittest, "TextTestRunner") as runner, contextlib.redirect_stdout(io.StringIO()):
            runner.return_value.run.return_value.wasSuccessful.return_value = True
            self.assertEqual(main(), 0)
            runner.return_value.run.assert_called_once()
            commands.assert_not_called()

    def test_explicit_qualification_uses_the_full_selection_and_both_subjects(self):
        # Q1 mode package/semver x default/all, with a Git-blocked SDK provider
        # and selected non-SDK/unpublished snapshots. The shared selection keeps
        # both SDKs (including the blocker), excludes the latter two, and the
        # original sequence runs exactly two feature subjects. Archive fixtures
        # prove record copying only, not a Cargo/package qualification receipt.
        for qualification in ("package", "semver"):
            with self.subTest(qualification=qualification), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary), successor=True)
                status, calls = self.execute(root, qualification=qualification)
                self.assertEqual(status, 0)
                commands = [c for c in calls if c[1] in ("+1.96.0", "semver-checks") and c[0] == "cargo"]
                self.assertEqual(len(commands), 2)
                for command in commands:
                    self.assertEqual(command[-4:], ["-p", "fixture-contract", "-p", "fixture-provider"])
                    self.assertIn("--target", command)
                    self.assertIn("x86_64-unknown-linux-gnu", command)
                self.assertNotIn("--all-features", commands[0])
                self.assertIn("--all-features", commands[1])
                if qualification == "package":
                    self.assertIn("--locked", commands[0])
                    self.assertNotIn("--no-verify", commands[0])
                    self.assertNotIn("--allow-dirty", commands[0])
                else:
                    self.assertIn("--default-features", commands[0])
                    self.assertNotIn("--locked", commands[0], "the original tool has no such flag")
                evidence = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                data = json.loads(evidence.read_text())
                self.assertEqual(data["exit_code"], 0)
                self.assertEqual(data["source"]["revision"], subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=root, text=True).strip())
                if qualification == "package":
                    self.assertEqual(set(data["artifacts"]), {"default", "all"})
                    for artifacts in data["artifacts"].values():
                        self.assertEqual(len(artifacts), 2)
                        for artifact in artifacts:
                            self.assertEqual(hashlib.sha256(Path(artifact["path"]).read_bytes()).hexdigest(), artifact["sha256"])
                else:
                    self.assertEqual(data["baseline_kind"], "first pre-release source contract")
                    self.assertNotEqual(data["source"]["revision"], data["baseline_rev"])

    def test_archive_extraction_rejects_aliases_escape_and_changed_bytes(self):
        # A4: exact .crate bytes + ordinary unique files => one unpacked peer.
        # Changed hash, escaped/wrong prefix, duplicate names, symlink/hardlink,
        # or missing manifest => refusal, with no write outside the fresh
        # extraction directory. No Cargo/process/source identity is mocked by
        # this real archive IO table; capture_archives owns VCS acceptance.
        for cause in ("valid", "checksum", "escape", "prefix", "duplicate", "symlink", "hardlink", "manifest"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                archive = directory / "peer-1.0.0.crate"
                member = "peer-1.0.0/Cargo.toml" if cause != "manifest" else "peer-1.0.0/src/lib.rs"
                with tarfile.open(archive, "w:gz") as package:
                    content = b'[package]\nname="peer"\nversion="1.0.0"\n'
                    item = tarfile.TarInfo(member)
                    item.size = len(content)
                    package.addfile(item, io.BytesIO(content))
                    if cause in ("escape", "prefix", "duplicate", "symlink", "hardlink"):
                        item = tarfile.TarInfo({"escape": "../escape", "prefix": "other/Cargo.toml"}.get(
                            cause, member if cause == "duplicate" else "peer-1.0.0/link"))
                        if cause in ("symlink", "hardlink"):
                            item.type = tarfile.SYMTYPE if cause == "symlink" else tarfile.LNKTYPE
                            item.linkname = "../../escape"
                        package.addfile(item)
                artifact = {"package": "peer", "path": str(archive), "sha256":
                            "0" * 64 if cause == "checksum" else hashlib.sha256(archive.read_bytes()).hexdigest()}
                if cause == "valid":
                    unpacked = _sdk_archives.unpack_archives([artifact], {"peer": {"version": "1.0.0"}}, directory / "out")
                    self.assertEqual((unpacked["peer"] / "Cargo.toml").read_bytes(), content)
                else:
                    with self.assertRaises(ValueError):
                        _sdk_archives.unpack_archives([artifact], {"peer": {"version": "1.0.0"}}, directory / "out")
                self.assertFalse((directory / "escape").exists())

    def test_artifact_manifest_preserves_fields_with_authored_path_and_version(self):
        # A6 causes: the original independent consumer declares a path alone or
        # a path with its release constraint, in any inline field order; an alias
        # and feature flags remain authored input. Effects: exactly one exact
        # archive version replaces the location/constraint, all other fields and
        # original task bytes remain identical. No duplicate TOML key or source
        # checkout fallback may be generated. These IO rows own transformation;
        # the real archive Cargo metadata still owns actual dependency resolution.
        fields = (
            'path="../../provider",package="fixture-provider",default-features=false,features=["first","version"]',
            'version="2.0",path="../../provider",package="fixture-provider",default-features=false,features=["first","version"]',
            'path="../../provider",package="fixture-provider",default-features=false,features=["first","version"],version="2.0"',
            'package="fixture-provider",path="../../provider",version="2.0",default-features=false,features=["first","version"]',
            'package="fixture-provider",default-features=false,features=["first","version"],version="2.0",path="../../provider"',
        )
        for declaration in fields:
            with self.subTest(declaration=declaration), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                original = root / "original"
                for directory in ("src", "tests"):
                    (original / directory).mkdir(parents=True)
                    (original / directory / "original.rs").write_text("// original consumer bytes\n")
                manifest = original / "Cargo.toml"
                manifest.write_text('[package]\nname="consumer"\nversion="0.0.0"\n[workspace]\n'
                                    '[dev-dependencies]\nadapter={' + declaration + '}\n')
                expected = tomllib.loads(manifest.read_text())
                dependency = expected["dev-dependencies"]["adapter"]
                dependency.pop("path")
                dependency["version"] = "=2.0.0"
                archive = root / "fixture-provider-2.0.0"
                expected["patch"] = {"crates-io": {"fixture-provider": {"path": str(archive)}}}
                staged = _sdk_archives.stage_consumer(
                    manifest, root / "consumer", {"fixture-provider": archive},
                    {"fixture-provider": {"version": "2.0.0"}},
                )
                self.assertEqual(tomllib.loads(staged.read_text()), expected)
                for directory in ("src", "tests"):
                    self.assertEqual(_sdk_archives.file_hashes(original / directory),
                                     _sdk_archives.file_hashes(staged.parent / directory))

    def test_artifact_mode_executes_the_original_tasks_and_sample_from_the_full_closure(self):
        # A1: clean source + SDK selection (including the blocked provider) +
        # original consumer Platform path => package the three fixture peers,
        # then resolve the independent graph, execute all eight exact tasks and
        # run the original sample. Effects: archive/VCS/graph/lock/source hashes
        # retained; no checkout import or private registry. This controlled IO
        # case validates orchestration only; real Cargo receipts are separate.
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary))
            status, calls = self.execute(root, qualification="artifact")
            self.assertEqual(status, 0)
            self.assertEqual([c[1] for c in calls], ["metadata", "+1.96.0", "metadata", "test", "run"])
            package = calls[1]
            self.assertNotIn("--no-verify", package)
            self.assertNotIn("--allow-dirty", package)
            self.assertEqual(package[-6:], ["-p", "fixture-contract", "-p", "fixture-internal", "-p", "fixture-provider"])
            receipt = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
            data = json.loads(receipt.read_text())
            self.assertEqual(data["sdk_crates"], ["fixture-contract", "fixture-provider"])
            self.assertEqual(data["consumed_packages"], data["artifact_packages"])
            self.assertEqual(data["executed_tasks"], sorted(EXPECTED))
            for command in calls[2:]:
                self.assertIn("x86_64-unknown-linux-gnu", command)
                manifest = Path(command[command.index("--manifest-path") + 1])
                self.assertEqual(manifest.parent, receipt.parent / "consumer")
            for directory in ("src", "tests"):
                self.assertEqual(_sdk_archives.file_hashes(root / "examples/sdk-consumer" / directory),
                                 _sdk_archives.file_hashes(receipt.parent / "consumer" / directory))

    def test_artifact_metadata_json_and_diagnostics_keep_the_original_protocol(self):
        # A7 cause/effect table from actual archive2729: C1 metadata stdout is
        # valid/invalid JSON; C2 normal index/unused-patch diagnostics are stderr;
        # C3 Cargo returns0/75. R1 valid+C2+0 => consume all eight original tasks
        # and sample; R2 invalid+C2+0 => reject1 before either successor;
        # R3 valid+C2+75 => preserve75. Preserve both raw channels without prefix
        # stripping, quieting diagnostics, or relabelling the child result.
        # Controlled IO only: real package/consumer qualification runs separately.
        for cause, raw, expected, count in (
            ("warnings", 0, 0, 5),
            ("warnings-json", 0, 1, 3),
            ("warnings", 75, 75, 3),
        ):
            with self.subTest(cause=cause, raw=raw), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary), successor=True)
                status, calls = self.execute(root, qualification="artifact",
                                             metadata_cause=cause, artifact_metadata_status=raw)
                self.assertEqual((status, len(calls)), (expected, count))
                evidence = next((root / "target/test-timings").glob("sdk-qualification-*"))
                receipt = json.loads((evidence / "commands.json").read_text())
                self.assertEqual(receipt["commands"][1]["exit_code"], raw)
                self.assertEqual(receipt["exit_code"], expected)
                stdout = (evidence / "metadata.stdout.log").read_text()
                stderr = (evidence / "metadata.stderr.log").read_text()
                self.assertEqual(stderr, "    Updating crates.io index\nwarning: patch fixture was not used\n")
                self.assertEqual((evidence / "metadata.log").read_text(), stdout + stderr)
                if cause == "warnings-json":
                    self.assertEqual(stdout, "broken-json")
                else:
                    self.assertIn("workspace_root", json.loads(stdout))
                if expected == 0:
                    self.assertEqual(receipt["executed_tasks"], sorted(EXPECTED))
                    self.assertEqual(calls[-1][1], "run")

    def test_artifact_graph_and_byte_faults_stop_before_the_next_command(self):
        # A2: successful package but SDK checkout/registry/version fallback =>
        # refusal before tasks. A3: archive/copied source or locked graph changes
        # at tasks => refusal before sample. Cross each successful step with a
        # nonzero original status: 75/143 keeps precedence over secondary byte
        # evidence faults. Existing Q3 owns malformed archives/VCS in the same
        # extracted recorder, E5 owns exact task completeness.
        for cause in ("artifact-checkout", "artifact-registry", "artifact-version", "artifact-duplicate",
                      "artifact-unreached", "artifact-shape"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary))
                status, calls = self.execute(root, qualification="artifact", output_cause=cause)
                self.assertEqual(status, 1)
                self.assertEqual([c[1] for c in calls], ["metadata", "+1.96.0", "metadata"])
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary))
            status, calls = self.execute(root, qualification="artifact", output_cause="missing")
            self.assertEqual(status, 1, "A5 a missing original task is not archive consumption")
            self.assertNotIn("run", [c[1] for c in calls])
        for cause in ("artifact-source", "artifact-consumer", "artifact-lock"):
            for raw, expected in ((0, 1), (75, 75), (-15, 143)):
                with self.subTest(cause=cause, raw=raw), tempfile.TemporaryDirectory() as temporary:
                    root = self.repository(Path(temporary))
                    # Earlier package/metadata must succeed. The controlled edge
                    # selects the test failure below rather than emulating Cargo.
                    status, calls = self.execute(root, qualification="artifact", phase="test",
                                                 output_cause=cause, test_status=raw)
                    self.assertEqual(status, expected)
                    self.assertNotIn("run", [c[1] for c in calls])

    def test_qualification_refusals_preserve_first_failure_and_start_no_successor(self):
        # Q2 metadata/first qualifier x 23/75/-15 -> same23/75/143, zero next
        # qualifier. Q3 success + Source/evidence/missing member/corrupt archive
        # fault ->1; an original failure
        # outranks that secondary fault. Q4 empty/malformed/selection/membership
        # preflight ->1, no qualifier. Full22 genai101 remains a real Root run,
        # not an exclusion or a claim made by this controlled IO table.
        for phase in ("metadata", "package", "semver-checks"):
            for raw, expected in ((23, 23), (75, 75), (-15, 143)):
                for mutation in ("", "working"):
                    with self.subTest(phase=phase, raw=raw, mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                        root = self.repository(Path(temporary), successor=True)
                        arguments = {"metadata_status" if phase == "metadata" else "test_status": raw}
                        qualification = "semver" if phase == "semver-checks" else "package"
                        status, calls = self.execute(root, qualification=qualification, phase=phase,
                                                     mutation=mutation, **arguments)
                        self.assertEqual(status, expected)
                        qualifiers = [c for c in calls if c[0] == "cargo" and c[1] in ("+1.96.0", "semver-checks")]
                        self.assertEqual(len(qualifiers), 0 if phase == "metadata" else 1)
                        if phase != "metadata":
                            evidence = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                            self.assertEqual(json.loads(evidence.read_text())["exit_code"], expected)
                            self.assertTrue(evidence.with_name("default.log").exists())
        for cause in ("json", "shape", "workspace", "members", "empty", "selection"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary))
                status, calls = self.execute(root, qualification="package", metadata_cause=cause)
                self.assertEqual(status, 1)
                self.assertFalse(any(c[1] == "+1.96.0" for c in calls))
        # Q3b standard VCS JSON must bind source SHA, clean/omitted dirty=false
        # and metadata manifest location. Stale/dirty/wrong/malformed VCS refuses
        # zero tool output before a second feature; original failures still win.
        for cause in ("source", "output", "archive", "member", "corrupt",
                      "vcs-sha", "vcs-dirty", "vcs-path", "vcs-malformed"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary))
                status, calls = self.execute(root, qualification="package", phase="package",
                    mutation="working" if cause == "source" else "", fault="output" if cause == "output" else "",
                    output_cause=cause if cause not in ("source", "output") else "")
                self.assertEqual(status, 1)
                self.assertEqual(len([c for c in calls if c[1] == "+1.96.0"]), 1)
        # Q3c metadata plan must match checked+finished package identities
        # exactly once. Whole-crate skip/zero/missing/duplicate/unchecked/zero
        # checks ->1 even with tool0; ordinary nonapplicable lint skips remain
        # valid. Failure101/75 outranks even these incomplete observations.
        for cause in ("semver-empty", "semver-missing", "semver-duplicate", "semver-unchecked", "semver-zero-checks"):
            for raw, expected in ((0, 1), (101, 101), (75, 75)):
                with self.subTest(cause=cause, raw=raw), tempfile.TemporaryDirectory() as temporary:
                    root = self.repository(Path(temporary), successor=True)
                    status, calls = self.execute(root, qualification="semver", phase="semver-checks",
                                                 test_status=raw, output_cause=cause)
                    self.assertEqual(status, expected)
                    self.assertEqual(len([c for c in calls if c[0] == "cargo" and c[1] == "semver-checks"]), 1)
        for axis in ("index", "working", "untracked"):
            with self.subTest(axis=axis), tempfile.TemporaryDirectory() as temporary:
                root = self.repository(Path(temporary))
                if axis == "index":
                    (root / "tracked.txt").write_text("index\n")
                    self.git(root, "add", "tracked.txt")
                    (root / "tracked.txt").write_text("before\n")
                elif axis == "working":
                    (root / "tracked.txt").write_text("dirty\n")
                else:
                    (root / "untracked.txt").write_text("dirty\n")
                status, calls = self.execute(root, qualification="package")
                self.assertEqual((status, calls), (1, []))

    def test_semver_requires_explicit_source_baseline_and_certain_tool(self):
        # Q5 absent/short/symbolic/nonexistent/HEAD/nonancestor baseline ->1,
        # zero metadata or qualifiers. Q6 undeclared/unpinned tool or declared
        # tool absent/undecidable/wrong
        # version/nonzero matching text ->1, no metadata; canonical M5 owns probe
        # classification. An accepted old commit is labelled pre-release, never
        # previous registry release; Q1 already covers both real feature argv.
        with tempfile.TemporaryDirectory() as temporary:
            root = self.repository(Path(temporary), successor=True)
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            decoy = self.repository(Path(temporary), "decoy")
            unrelated = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=decoy, text=True).strip()
            self.git(root, "fetch", "--quiet", str(decoy), unrelated)
            for baseline in ("", "HEAD", head[:8], "f" * 40, head, unrelated):
                with self.subTest(baseline=baseline):
                    status, calls = self.execute(root, qualification="semver", baseline_rev=baseline)
                    self.assertEqual((status, calls), (1, []))
            for cause in ("undeclared", "pin", "absent", "undecidable", "version", "query"):
                with self.subTest(tool=cause):
                    status, calls = self.execute(root, qualification="semver", tool_cause=cause)
                    self.assertEqual(status, 1)
                    self.assertFalse(any(c[1] in ("metadata", "semver-checks") and c[0] == "cargo" for c in calls))

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
            # TMP may live inside the caller's checkout. An unresolved local
            # marker makes this a real Git128 source instead of finding its parent.
            (empty / ".git").write_text("gitdir: missing-git-directory\n")
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
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--qualify-package", action="store_true")
    mode.add_argument("--qualify-artifact", action="store_true")
    mode.add_argument("--qualify-semver", action="store_true")
    mode.add_argument("--major-consumer-audit", type=Path)
    parser.add_argument("--baseline-rev")
    args = parser.parse_args()
    if args.qualify_semver != bool(args.baseline_rev):
        parser.error("--baseline-rev is required only with --qualify-semver")
    self_test()
    if args.self_test:
        from sdk_semver_tests import SdkSemverEvidenceTests, shared_sdk_io_tests, compiler_input_tests
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SdkConsumerTests)
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(SdkSemverEvidenceTests))
        suite.addTests(shared_sdk_io_tests(sys.modules[__name__]))
        suite.addTests(compiler_input_tests(sys.modules[__name__]))
        if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
            return 1
        print("SDK consumer self-test: admission paths and exact task execution are enforced")
        return 0
    if args.major_consumer_audit:
        return run_major_consumer_audit(args.major_consumer_audit)
    qualification = ("package" if args.qualify_package else "artifact" if args.qualify_artifact
                     else "semver" if args.qualify_semver else "")
    status = run_consumer(qualification=qualification, baseline_rev=args.baseline_rev or "")
    if status:
        return status
    if qualification == "artifact":
        print(f"SDK artifact qualification: original {len(EXPECTED)} tasks and sample passed from verified archives on Linux")
    elif qualification:
        print(f"SDK {qualification} qualification: both Linux feature subjects and their required change audits passed")
    else:
        print(f"SDK consumer: all {len(EXPECTED)} task cases passed in an independent minimal-feature workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
