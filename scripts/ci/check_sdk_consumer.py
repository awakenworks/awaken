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
import shutil
import subprocess
import sys
import tempfile
import tarfile
import unittest
from unittest import mock

import crate_test
import gate_tools
import check_gate_freshness
from public_api_selection import selected_crates
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
    "web_policy_configuration_remains_usable_through_sdk_reexports",
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


def validate_semver_evidence(output: str, expected: list[str]) -> None:
    """Match pinned 0.47 checked/finished observations to the planned packages."""
    # Preserve raw logs; colors do not change the pinned terminal vocabulary.
    output = re.sub(r"\x1b\[[0-9;]*m", "", output)
    current = None
    checked = False
    finished = []
    for line in output.splitlines():
        start = re.fullmatch(r"\s*Checking ([A-Za-z0-9_-]+) v.+", line)
        result = re.fullmatch(r"\s*Checked \[[^\]]+\] (\d+) checks: (\d+) pass(?:, (\d+) skip)?", line)
        end = re.fullmatch(r"\s*Finished \[[^\]]+\] ([A-Za-z0-9_-]+)", line)
        if start:
            if current is not None:
                raise ValueError("SDK SemVer package finished without checked evidence")
            current, checked = start.group(1), False
        elif result:
            if current is None or checked or int(result.group(1)) == 0 or result.group(1) != result.group(2):
                raise ValueError("SDK SemVer checked evidence is empty, duplicate or unsuccessful")
            checked = True
        elif end:
            if current != end.group(1) or not checked:
                raise ValueError("SDK SemVer finished evidence has no matching checked package")
            finished.append(current)
            current, checked = None, False
    if current is not None or len(finished) != len(expected) or set(finished) != set(expected) or len(set(finished)) != len(finished):
        raise ValueError(f"SDK SemVer execution is incomplete: expected={expected}, finished={finished}")


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
    document = {"qualification": qualification, "target": "x86_64-unknown-linux-gnu",
                "baseline_kind": "first pre-release source contract" if baseline_rev else None,
                "baseline_rev": baseline_rev or None}

    def verify() -> None:
        nonlocal subject
        observed, facts = rust_test_plan.working_subject()
        if qualification and (facts["status"] or facts["staged"] or facts["working"] or facts["untracked"]):
            raise ValueError("SDK qualification requires an exact clean source/index/worktree")
        if subject is None:
            subject = observed
            if qualification:
                document["source"] = {"revision": facts["revision"].decode().strip(),
                                      "tree": facts["tree"].decode().strip(),
                                      "index_tree": facts["index_tree"].decode().strip(),
                                      "working_subject": observed}
        elif observed != subject:
            raise ValueError("SDK consumer source changed before or during execution")

    def capture(command, **arguments):
        # Only preserve the existing stdio contract. No retry, deadline, process
        # manager or command loop: Cargo admission and lifecycle stay upstream.
        nonlocal completed
        completed = None
        stdio = {"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT} if qualification else {"capture_output": True}
        completed = subprocess.run(command, text=True, **stdio, **arguments)
        if qualification and completed.stderr is None:
            completed.stderr = ""
        return completed

    def record(rows) -> None:
        if completed is None:
            return
        if rows[-1]["command"][1] == "test":
            sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        if qualification:
            command = rows[-1]["command"]
            feature = "all" if "--all-features" in command else "default"
            (evidence / (feature + ".log")).write_text(completed.stdout + completed.stderr)
            if rows[-1]["exit_code"] == 0 and qualification == "semver":
                validate_semver_evidence(completed.stdout + completed.stderr, document["sdk_crates"])
            if rows[-1]["exit_code"] == 0 and qualification == "package":
                artifacts = []
                archive_directory = Path(values["CARGO_BUILD_BUILD_DIR"])
                if not archive_directory.is_absolute():
                    archive_directory = ROOT / archive_directory
                archive_directory /= "package"
                for name in document["sdk_crates"]:
                    archive = archive_directory / f"{name}-{members[name]['version']}.crate"
                    data = archive.read_bytes()
                    destination = evidence / (feature + "-artifacts")
                    destination.mkdir(exist_ok=True)
                    (destination / archive.name).write_bytes(data)
                    normalized = {}
                    try:
                        package = tarfile.open(archive, "r:gz")
                    except tarfile.TarError as error:
                        raise ValueError(f"unreadable package archive: {name}") from error
                    with package:
                        for item in ("Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"):
                            try:
                                reader = package.extractfile(f"{name}-{members[name]['version']}/{item}")
                            except KeyError as error:
                                raise ValueError(f"missing normalized package member: {name}/{item}") from error
                            if reader is None:
                                raise ValueError(f"missing normalized package member: {name}/{item}")
                            contents = reader.read()
                            (destination / f"{name}--{item}").write_bytes(contents)
                            normalized[item] = hashlib.sha256(contents).hexdigest()
                            if item == ".cargo_vcs_info.json":
                                vcs = json.loads(contents)
                                if not isinstance(vcs, dict) or not isinstance(vcs.get("git"), dict):
                                    raise ValueError(f"invalid package VCS observation: {name}")
                                manifest_path = members[name].get("manifest_path")
                                if not isinstance(manifest_path, str):
                                    raise ValueError(f"package metadata has no manifest location: {name}")
                                expected_path = Path(manifest_path).parent.resolve().relative_to(ROOT.resolve()).as_posix()
                                # Cargo 1.96 omits git.dirty when it is false.
                                if (vcs["git"].get("sha1") != document["source"]["revision"]
                                        or vcs["git"].get("dirty", False) is not False
                                        or vcs.get("path_in_vcs") != expected_path):
                                    raise ValueError(f"package VCS observation does not bind the clean source: {name}")
                    artifacts.append({"package": name, "path": str(destination / archive.name),
                                      "sha256": hashlib.sha256(data).hexdigest(), "normalized": normalized})
                document.setdefault("artifacts", {})[feature] = artifacts
            document["commands"] = rows
            (evidence / "commands.json").write_text(json.dumps(document, indent=2) + "\n")
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
            target = Path(values["CARGO_BUILD_BUILD_DIR"])
            if not target.is_absolute():
                target = ROOT / target
            target /= "test-timings"
            target.mkdir(parents=True, exist_ok=True)
            evidence = Path(tempfile.mkdtemp(prefix="sdk-qualification-", dir=target))
            (evidence / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
            selectors = [word for name in sdk for word in ("-p", name)]
            if qualification == "package":
                command = ["cargo", "+1.96.0", "package", "--locked", "--target", document["target"]]
                commands = [command + selectors, command + ["--all-features"] + selectors]
            else:
                command = ["cargo", "semver-checks", "check-release", "--baseline-rev", baseline_rev,
                           "--target", document["target"]]
                commands = [command + [features] + selectors
                            for features in ("--default-features", "--all-features")]
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
                                           root=ROOT, verify=verify, record=record, run=capture)
    if qualification:
        document["commands"] = rows
        document["exit_code"] = status
        try:
            (evidence / "commands.json").write_text(json.dumps(document, indent=2) + "\n")
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
                qualification="", baseline_rev=None, tool_cause="") -> tuple[int, list]:
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
            metadata = {"workspace_root": str(root), "workspace_members": [p["id"] for p in packages],
                        "packages": packages,
                        "resolve": {"nodes": [{"id": p["id"], "deps": []} for p in packages]}}
            if baseline_rev is None:
                baseline_rev = subprocess.check_output(["git", "rev-parse", "HEAD^"], cwd=root,
                    env=formal_sources.detached_git_environment(), text=True).strip() if qualification == "semver" else ""
            revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                env=formal_sources.detached_git_environment(), text=True).strip()
            for package in packages:
                package["manifest_path"] = str(root / "crates" / package["name"] / "Cargo.toml")
            if output_cause != "archive":
                # Test-owned archive data exercises only the record callback;
                # the IO mock below never implements Cargo or grants real package
                # qualification. Real Source/artifact receipts remain Root's.
                archive_root = root / "target/package"
                archive_root.mkdir(parents=True, exist_ok=True)
                for package in packages[:2]:
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
            if program not in ("cargo", "cargo-semver-checks"):
                return actual_run(command, **arguments)
            if program == "cargo":
                self.assertEqual(arguments["cwd"], root)
                self.assertEqual(arguments["env"]["CARGO_BUILD_BUILD_DIR"], arguments["env"]["CARGO_TARGET_DIR"])
            else:
                self.assertEqual(command[1:], ["semver-checks", "--version"])
            current_phase = ("package" if command[1] == "+1.96.0" else
                             "tool" if program == "cargo-semver-checks" else command[1])
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
            stdout = ("broken-json" if metadata_cause == "json" else json.dumps(metadata)) if current_phase == "metadata" else output
            if current_phase == "tool":
                status = 23 if tool_cause == "query" else 0
                stdout = "cargo-semver-checks 0.46.0\n" if tool_cause == "version" else "cargo-semver-checks 0.47.0\n"
            stderr = "SDK captured diagnostic\n" if current_phase == phase else ""
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
                          ["--self-test", "--qualify-package"], ["--qualify-packag"]):
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
    mode.add_argument("--qualify-semver", action="store_true")
    parser.add_argument("--baseline-rev")
    args = parser.parse_args()
    if args.qualify_semver != bool(args.baseline_rev):
        parser.error("--baseline-rev is required only with --qualify-semver")
    self_test()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SdkConsumerTests)
        if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
            return 1
        print("SDK consumer self-test: admission paths and exact task execution are enforced")
        return 0
    qualification = "package" if args.qualify_package else "semver" if args.qualify_semver else ""
    status = run_consumer(qualification=qualification, baseline_rev=args.baseline_rev or "")
    if status:
        return status
    if qualification:
        print(f"SDK {qualification} qualification: all selected SDK crates passed both Linux feature subjects")
    else:
        print(f"SDK consumer: all {len(EXPECTED)} task cases passed in an independent minimal-feature workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
