#!/usr/bin/env python3
"""Derive local affected tests from the actual Cargo graph; never bless a full gate."""
from __future__ import annotations

import argparse
from collections import deque
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock
import formal_sources

ROOT = Path(__file__).resolve().parents[2]
PROFILE = ROOT / ".config/rust-tests.toml"
INPUT_GLOBS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates/**", ".config/**", "scripts/ci/**")


def workspace_features() -> str:
    features = tomllib.loads(PROFILE.read_text(encoding="utf-8"))["workspace_features"]
    if not isinstance(features, list) or not features or any(not isinstance(f, str) or not f for f in features) or len(set(features)) != len(features):
        raise ValueError("workspace_features must be a nonempty unique string list")
    return ",".join(features)


def affected_packages(metadata: dict, changed: list[str], root: Path = ROOT) -> tuple[list[str], str]:
    member_ids = metadata["workspace_members"]
    members = set(member_ids)
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    local_records = [p for p in metadata["packages"] if p["id"] in members]
    if not members or len(members) != len(member_ids) or set(packages) != members or len(local_records) != len(members):
        raise ValueError("Cargo metadata must contain every unique workspace member")
    if metadata.get("resolve") is None:
        raise ValueError("Cargo's resolved feature/dependency graph is required")
    nodes = metadata["resolve"]["nodes"]
    node_ids = [node["id"] for node in nodes]
    if len(set(node_ids)) != len(node_ids) or not members <= set(node_ids):
        raise ValueError("Cargo's resolved graph omits or duplicates a workspace member")
    owners = sorted(((Path(p["manifest_path"]).parent.relative_to(root).as_posix(), identity) for identity, p in packages.items()), reverse=True)
    selected: set[str] = set()
    for path in changed:
        if path.startswith("docs/") and path.endswith(".md"):
            continue
        if Path(path).name == "Cargo.toml":
            # Changed features/membership/edges cannot be inferred from only
            # the new graph, even when this manifest still has a current owner.
            return sorted(p["name"] for p in packages.values()), "manifest input: " + path
        owner = next((identity for directory, identity in owners if path == directory or path.startswith(directory + "/")), None)
        if owner is None:
            return sorted(p["name"] for p in packages.values()), "global or unowned input: " + path
        selected.add(owner)
    if not selected:
        return [], "no changed Rust ownership boundary; this is not a full-gate result"
    reverse: dict[str, set[str]] = {identity: set() for identity in members}
    for node in nodes:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    reverse[dependency["pkg"]].add(node["id"])
    pending = deque(selected)
    while pending:
        for consumer in reverse[pending.popleft()] - selected:
            selected.add(consumer)
            pending.append(consumer)
    return sorted(packages[identity]["name"] for identity in selected), "changed owners and transitive consumers, including test dependencies"


def git_output(*arguments: str) -> bytes:
    return formal_sources.git_output(ROOT, *arguments)


def source_digest(revision: bytes, index: bytes, staged: bytes, working: bytes, untracked: dict[str, bytes]) -> str:
    return formal_sources.framed_digest((revision, index, staged, working, *formal_sources.untracked_parts(untracked)))


def untracked_sources() -> dict[str, bytes]:
    """The one byte reader used by subject identity and ordinary diagnostics."""
    return formal_sources.untracked_sources(ROOT)


def source_snapshot() -> tuple[str, bytes]:
    """Working subject plus its exact index; retain the existing digest/patch API.

    Index and WT deltas are independent: they may cancel in HEAD->WT. The
    returned diagnostic patch remains HEAD->WT; its digest also binds the index,
    both deltas and untracked bytes. A local run never qualifies a staged commit.
    """
    facts = formal_sources.source_facts(ROOT)
    index = git_output("write-tree")
    if facts != formal_sources.source_facts(ROOT):
        raise ValueError("Source changed while capturing the working subject and index")
    return source_digest(facts["revision"], index, facts["staged"], facts["working"], facts["untracked"]), facts["patch"]


def changed_sources(base: str) -> list[str]:
    """Union index, WT and untracked causes; rename deletion/addition both count."""
    return sorted(set(
        os.fsdecode(git_output("diff", "--cached", "--no-renames", "--name-only", "-z", base, "--")).split("\0")
        + os.fsdecode(git_output("diff", "--no-renames", "--name-only", "-z", "--")).split("\0")
        + os.fsdecode(git_output("ls-files", "--others", "--exclude-standard", "-z")).split("\0")
    ) - {""})


class PlanTests(unittest.TestCase):
    def setUp(self) -> None:
        # Reuse the existing Git-environment authority. A hook exports its own
        # repository/index; no throwaway fixture may initialize or configure it.
        from formal_sources import detached_git_environment
        self.detached_environment = detached_git_environment
        environment = mock.patch.dict(os.environ, detached_git_environment(), clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def fixture_repository(self, directory: str) -> tuple[Path, dict]:
        root = Path(directory) / "repository"
        root.mkdir()
        environment = dict(self.detached_environment(), GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.git(root, "init", "--quiet", environment=environment)
        self.git(root, "config", "user.name", "planner test", environment=environment)
        self.git(root, "config", "user.email", "planner@example.invalid", environment=environment)
        self.git(root, "config", "commit.gpgsign", "false", environment=environment)
        self.git(root, "config", "core.hooksPath", str(Path(directory) / "no-hooks"), environment=environment)
        from formal_plan import script_closure
        for relative in script_closure(ROOT, ("scripts/ci/rust_test_plan.py",)):
            script = root / relative
            script.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, script)
        profile = root / ".config/rust-tests.toml"
        profile.parent.mkdir()
        profile.write_text('workspace_features = ["kernel/input"]\n', encoding="utf-8")
        names = ("kernel", "application", "cli", "other")
        for name in names:
            manifest = root / "crates" / name / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(f'[package]\nname = "{name}"\n', encoding="utf-8")
            source = manifest.parent / "src/lib.rs"
            source.parent.mkdir()
            source.write_text("pub const VALUE: u8 = 1;\n", encoding="utf-8")
        self.git(root, "add", ".", environment=environment)
        self.git(root, "commit", "--quiet", "-m", "fixture", environment=environment)
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(root / "crates" / n / "Cargo.toml")} for n in names],
            "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in deps]} for n, deps in (("kernel", ()), ("application", ("kernel",)), ("cli", ("application",)), ("other", ()))]},
        }
        return root, metadata

    def git(self, root: Path, *arguments: str, environment: dict | None = None) -> bytes:
        return subprocess.check_output(["git", "-C", str(root), *arguments],
                                       env=self.detached_environment() if environment is None else environment, stderr=subprocess.PIPE)

    def planner_cli(self, directory: str, root: Path, metadata: dict, *, metadata_status: int = 0,
                    execution_status: int = 0, mutate: str = "", broken_metadata: bool = False,
                    run: bool = True) -> tuple[subprocess.CompletedProcess, list[str], list[dict]]:
        # The fixture controls the original subprocess boundary, not Cargo's
        # dependency solver or compile behavior. Real Git supplies the subject.
        outer = Path(directory)
        tools = outer / "tools"
        tools.mkdir(exist_ok=True)
        metadata_path, calls_path = outer / "metadata.json", outer / "calls.jsonl"
        metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
        shim = tools / "cargo_fixture.py"
        shim.write_text('''import json, os, pathlib, subprocess, sys
command = sys.argv[1]
with pathlib.Path(os.environ["W41_FIXTURE_CALLS"]).open("a") as out:
    out.write(json.dumps(command) + "\\n")
if command == "metadata":
    status = int(os.environ["W41_FIXTURE_METADATA_STATUS"])
    if status:
        print("fixture metadata refusal", file=sys.stderr)
        sys.exit(status)
    if os.environ["W41_FIXTURE_MUTATE"] == "metadata":
        pathlib.Path("crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\\n")
    print("{" if os.environ["W41_FIXTURE_BROKEN"] == "1" else pathlib.Path(os.environ["W41_FIXTURE_METADATA"]).read_text())
elif command == "nextest":
    if os.environ["W41_FIXTURE_MUTATE"] == "execution":
        pathlib.Path("crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\\n")
    if os.environ["W41_FIXTURE_MUTATE"] == "evidence":
        patch = next((pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "test-timings").glob("affected-*.patch"))
        patch.with_suffix(".json").mkdir()
    if os.environ["W41_FIXTURE_MUTATE"] == "unknown-source":
        path = "crates/kernel/src/lib.rs"
        blob = subprocess.check_output(["git", "hash-object", path]).decode().strip()
        subprocess.run(["git", "update-index", "--index-info"], input=f"100644 {blob} 1\\t{path}\\n".encode(), check=True)
    sys.exit(int(os.environ["W41_FIXTURE_EXECUTION_STATUS"]))
else:
    sys.exit(91)
''', encoding="utf-8")
        if os.name == "nt":
            (tools / "cargo.cmd").write_text(f'@"{sys.executable}" "{shim}" %*\n', encoding="utf-8")
        else:
            cargo = tools / "cargo"
            cargo.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{shim}" "$@"\n', encoding="utf-8")
            cargo.chmod(0o755)
        target = outer / "target"
        environment = dict(self.detached_environment(), PATH=str(tools) + os.pathsep + os.environ.get("PATH", ""),
                           CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                           W41_FIXTURE_METADATA=str(metadata_path), W41_FIXTURE_CALLS=str(calls_path),
                           W41_FIXTURE_METADATA_STATUS=str(metadata_status), W41_FIXTURE_EXECUTION_STATUS=str(execution_status),
                           W41_FIXTURE_MUTATE=mutate, W41_FIXTURE_BROKEN=str(int(broken_metadata)))
        result = subprocess.run([sys.executable, "-B", str(root / "scripts/ci/rust_test_plan.py"), "--base", "HEAD", *(["--run"] if run else [])],
                                cwd=root, env=environment, capture_output=True, text=True)
        calls = [json.loads(line) for line in calls_path.read_text().splitlines()] if calls_path.exists() else []
        receipts = [json.loads(path.read_text()) for path in (target / "test-timings").glob("affected-*.json") if path.is_file()]
        return result, calls, receipts

    def test_fixture_git_operations_ignore_the_hook_repository(self) -> None:
        # Q12: ambient hook GIT_DIR/INDEX/WORK_TREE designate a real decoy repo.
        # Both fixture construction and its actual CLI must use their own tree,
        # preserve the decoy config/index/HEAD/refs/WT, and retain the selection.
        # This catches destructive fixture contamination, not only CLI failure.
        with tempfile.TemporaryDirectory() as directory:
            outer = Path(directory)
            decoy_outer = outer / "decoy"
            decoy_outer.mkdir()
            decoy, _ = self.fixture_repository(str(decoy_outer))
            def protected_bytes():
                controls = [decoy / ".git" / name for name in ("config", "index", "HEAD")]
                refs = [path for path in (decoy / ".git/refs").rglob("*") if path.is_file()]
                working = [path for path in decoy.rglob("*") if path.is_file() and ".git" not in path.relative_to(decoy).parts]
                return {path.relative_to(decoy).as_posix(): path.read_bytes() for path in controls + refs + working}
            before = protected_bytes()
            subject = outer / "subject"
            subject.mkdir()
            with mock.patch.dict(os.environ, GIT_DIR=str(decoy / ".git"),
                                 GIT_INDEX_FILE=str(decoy / ".git/index"), GIT_WORK_TREE=str(decoy), GIT_PREFIX="hook/"):
                root, metadata = self.fixture_repository(str(subject))
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                result, calls, receipts = self.planner_cli(str(subject), root, metadata)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, ["metadata", "nextest"])
            self.assertEqual(receipts[0]["selected_packages"], ["application", "cli", "kernel"])
            self.assertEqual(protected_bytes(), before)

    def test_snapshot_distinguishes_index_worktree_and_untracked_sources(self) -> None:
        # Q9/Q10: same HEAD and same working bytes but changed index must change
        # identity; tracked WIP and untracked content are independent causes.
        # A staged change reverted only in WT cannot disappear from the subject.
        with tempfile.TemporaryDirectory() as directory:
            root, _ = self.fixture_repository(directory)
            source = root / "crates/kernel/src/lib.rs"
            with mock.patch.dict(globals(), ROOT=root):
                original = source_snapshot()[0]
                source.write_text("pub const VALUE: u8 = 2;\n")
                self.git(root, "add", str(source))
                source.write_text("pub const VALUE: u8 = 1;\n")
                index_changed = source_snapshot()[0]
                self.assertNotEqual(original, index_changed)
                self.git(root, "reset", "--quiet", "HEAD", "--", str(source))
                self.assertEqual(original, source_snapshot()[0])
                source.write_text("pub const VALUE: u8 = 2;\n")
                self.assertNotEqual(original, source_snapshot()[0])
                source.write_text("pub const VALUE: u8 = 1;\n")
                untracked = root / "crates/kernel/src/new.rs"
                untracked.write_bytes(b"one")
                one = source_snapshot()[0]
                untracked.write_bytes(b"two")
                self.assertNotEqual(one, source_snapshot()[0])

    def test_planning_cannot_lose_staged_work_reverted_in_worktree(self) -> None:
        # Q10: staged bad/different + WT matching HEAD used to return no work.
        # Local --run owns the working subject, but selection must retain both
        # index and WT causes, never claim qualification of the staged commit.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            source = root / "crates/kernel/src/lib.rs"
            source.write_text("pub const VALUE: u8 = 2;\n")
            self.git(root, "add", str(source))
            source.write_text("pub const VALUE: u8 = 1;\n")
            fresh = root / "crates/kernel/src/new.rs"
            fresh.write_bytes(b"pinned")
            result, calls, receipts = self.planner_cli(directory, root, metadata)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, ["metadata", "nextest"])
            self.assertEqual(receipts[0]["selected_packages"], ["application", "cli", "kernel"])
            self.assertEqual(receipts[0]["source_kind"], "working-tree")
            self.assertFalse(receipts[0]["full_gate"])
            self.assertEqual(receipts[0]["index_tree"], self.git(root, "write-tree").decode().strip())
            self.assertEqual(receipts[0]["untracked_files"], {"crates/kernel/src/new.rs": hashlib.sha256(b"pinned").hexdigest()})
            self.assertEqual(receipts[0]["command"], ["cargo", "nextest", "run", "--workspace", "--locked", "--features", "kernel/input", "-E", "package(=application) | package(=cli) | package(=kernel)"])

    def test_metadata_refusal_keeps_original_status_and_starts_no_execution(self) -> None:
        # Q7/Q8: original metadata status 75 or 23 -> exactly that CLI status,
        # one metadata invocation and zero execution/receipt promotion. This is
        # the actual CLI and real process exit, not a Boolean policy proxy.
        for status in (75, 23):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                result, calls, receipts = self.planner_cli(directory, root, metadata, metadata_status=status)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(calls, ["metadata"])
                self.assertEqual(receipts, [])

    def test_source_change_during_metadata_refuses_the_old_selection(self) -> None:
        # Q11: metadata begins with kernel-only input; another owner's bytes
        # change before it returns. Refuse before execution instead of binding
        # the stale selection to a later source fingerprint.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
            result, calls, receipts = self.planner_cli(directory, root, metadata, mutate="metadata")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Source changed during affected planning", result.stderr)
            self.assertEqual(calls, ["metadata"])
            self.assertEqual(receipts, [])

    def test_real_rename_deletion_untracked_and_manifest_causes_are_retained(self) -> None:
        # Q4/Q5/Q9: a staged cross-owner rename plus owned deletion and new
        # untracked Rust bytes retains both rename ends. Adding an owned
        # manifest changes the graph and explicitly widens to the full set.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            old, new = "crates/kernel/src/lib.rs", "crates/other/src/moved.rs"
            self.git(root, "mv", old, new)
            deleted = "crates/application/src/lib.rs"
            self.git(root, "rm", deleted)
            fresh = "crates/cli/src/new.rs"
            (root / fresh).write_text("pub const FRESH: bool = true;\n")
            with mock.patch.dict(globals(), ROOT=root):
                changed = changed_sources("HEAD")
                self.assertEqual(changed, sorted([old, new, deleted, fresh]))
                self.assertEqual(affected_packages(metadata, changed, root)[0], ["application", "cli", "kernel", "other"])
                (root / "crates/other/Cargo.toml").write_text('[package]\nname = "other"\n[features]\nnew = []\n')
                self.assertEqual(affected_packages(metadata, changed_sources("HEAD"), root)[0], ["application", "cli", "kernel", "other"])
                self.assertIn("manifest", affected_packages(metadata, ["crates/other/Cargo.toml"], root)[1])

    def test_broken_metadata_never_becomes_empty_work_or_a_receipt(self) -> None:
        # Q7: status0 is insufficient. Broken JSON, missing package, missing
        # resolve node, duplicate package and absent resolve all reject before
        # execution; a global manifest must not hide an incomplete graph.
        for cause in ("json", "package", "node", "duplicate", "resolve", "outside", "member"):
            with self.subTest(cause=cause), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/Cargo.toml").write_text('[package]\nname = "kernel"\n[features]\nnew = []\n')
                if cause == "package":
                    metadata["packages"].pop()
                elif cause == "node":
                    metadata["resolve"]["nodes"].pop()
                elif cause == "duplicate":
                    metadata["packages"].append(metadata["packages"][0])
                elif cause == "resolve":
                    metadata["resolve"] = None
                elif cause == "outside":
                    metadata["packages"][0]["manifest_path"] = str(Path(directory) / "outside/Cargo.toml")
                elif cause == "member":
                    metadata["workspace_members"].append("absent")
                result, calls, receipts = self.planner_cli(directory, root, metadata, broken_metadata=cause == "json")
                self.assertNotEqual(result.returncode, 0, cause)
                self.assertEqual(calls, ["metadata"])
                self.assertEqual(receipts, [])

    def test_unknown_source_cannot_be_masked_by_full_workspace_selection(self) -> None:
        # Q7/Q12: an unresolved index has no tree identity. It is rejected
        # before any Cargo call, even though the changed path has a known owner.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            identity = self.git(root, "hash-object", "crates/kernel/src/lib.rs").decode().strip()
            conflict = f"100644 {identity} 1\tcrates/kernel/src/lib.rs\n".encode()
            subprocess.run(["git", "-C", str(root), "update-index", "--index-info"], input=conflict, check=True)
            result, calls, receipts = self.planner_cli(directory, root, metadata)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            self.assertEqual(receipts, [])

    def test_execution_status_survives_source_and_receipt_failures(self) -> None:
        # Q8/Q11/Q12: cross actual execution status0/23/75 with changed bytes
        # or a failed receipt writer. Zero must become failure; original23/75
        # must survive the secondary error, never qualify the tree or retry.
        for cause in ("execution", "evidence", "unknown-source"):
            for status in (0, 23, 75):
                with self.subTest(cause=cause, status=status), tempfile.TemporaryDirectory() as directory:
                    root, metadata = self.fixture_repository(directory)
                    (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                    result, calls, receipts = self.planner_cli(directory, root, metadata, execution_status=status, mutate=cause)
                    self.assertEqual(result.returncode, status or 1, result.stderr)
                    self.assertEqual(calls, ["metadata", "nextest"])
                    if cause != "evidence":
                        self.assertFalse(receipts[0]["source_unchanged"])
                        self.assertEqual(receipts[0]["exit_code"], status or 1)
                    else:
                        self.assertEqual(receipts, [])

    def test_change_between_planning_and_execution_starts_no_execution(self) -> None:
        # Q11: an evidence-write boundary gives another writer time to amend
        # source after planning was verified. The final entrance checks that
        # same snapshot and refuses before polling the execution owner.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
            original_output, original_write, original_run = subprocess.check_output, Path.write_bytes, subprocess.run
            calls = []
            errors = io.StringIO()
            def output(command, **kwargs):
                if command[0] == "cargo":
                    calls.append(command[1])
                    return json.dumps(metadata).encode()
                return original_output(command, **kwargs)
            def write(path, data):
                result = original_write(path, data)
                if path.suffix == ".patch":
                    (root / "crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\n")
                return result
            def execute(command, **kwargs):
                if command[0] == "cargo":
                    calls.append(command[1])
                    return subprocess.CompletedProcess(command, 0)
                return original_run(command, **kwargs)
            with mock.patch.dict(globals(), ROOT=root, PROFILE=root / ".config/rust-tests.toml"), \
                    mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(Path(directory) / "target")), \
                    mock.patch.object(sys, "argv", [__file__, "--base", "HEAD", "--run"]), \
                    mock.patch.object(subprocess, "check_output", side_effect=output), \
                    mock.patch.object(Path, "write_bytes", write), \
                    mock.patch.object(subprocess, "run", side_effect=execute), contextlib.redirect_stdout(io.StringIO()), \
                    contextlib.redirect_stderr(errors):
                self.assertNotEqual(main(), 0)
            self.assertEqual(calls, ["metadata"])
            self.assertIn("Source changed before affected execution", errors.getvalue())

    def test_initial_evidence_failure_starts_no_execution(self) -> None:
        # Q12: the selected source is known, but its original .patch evidence
        # cannot be retained. Refuse before the first execution invocation.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
            original_output, original_run = subprocess.check_output, subprocess.run
            calls = []
            errors = io.StringIO()
            def output(command, **kwargs):
                if command[0] == "cargo":
                    calls.append(command[1])
                    return json.dumps(metadata).encode()
                return original_output(command, **kwargs)
            def execute(command, **kwargs):
                if command[0] == "cargo":
                    calls.append(command[1])
                    return subprocess.CompletedProcess(command, 0)
                return original_run(command, **kwargs)
            with mock.patch.dict(globals(), ROOT=root, PROFILE=root / ".config/rust-tests.toml"), \
                    mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(Path(directory) / "target")), \
                    mock.patch.object(sys, "argv", [__file__, "--base", "HEAD", "--run"]), \
                    mock.patch.object(subprocess, "check_output", side_effect=output), \
                    mock.patch.object(subprocess, "run", side_effect=execute), \
                    mock.patch.object(Path, "write_bytes", side_effect=OSError("fixture evidence refusal")), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(errors):
                self.assertNotEqual(main(), 0)
            self.assertEqual(calls, ["metadata"])
            self.assertIn("fixture evidence refusal", errors.getvalue())

    def test_plan_only_cannot_mix_late_index_or_revision_with_the_old_digest(self) -> None:
        # Q10/Q11: --base without --run still owns a source-bound plan. Initial
        # capture now delegates raw facts to the shared owner. The planner's
        # first HEAD read / second write-tree read is its diagnostic fact.
        # Cross late index/HEAD amendment there and require refusal before any
        # plan is printed, not just the later execution-entrance refusal.
        for axis in ("write-tree", "rev-parse"):
            with self.subTest(axis=axis), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                original_git, original_output = git_output, subprocess.check_output
                axis_reads = 0
                diagnostic_read = 2 if axis == "write-tree" else 1
                calls = []
                def git_read(*arguments):
                    nonlocal axis_reads
                    result = original_git(*arguments)
                    if arguments == (axis,) or arguments == (axis, "HEAD"):
                        axis_reads += 1
                        if axis_reads == diagnostic_read:
                            source = root / "crates/other/src/lib.rs"
                            source.write_text("pub const VALUE: u8 = 3;\n")
                            self.git(root, "add", str(source))
                            if axis == "rev-parse":
                                self.git(root, "commit", "--quiet", "-m", "late fixture revision")
                    return result
                def output(command, **kwargs):
                    if command[0] == "cargo":
                        calls.append(command[1])
                        return json.dumps(metadata).encode()
                    return original_output(command, **kwargs)
                printed, errors = io.StringIO(), io.StringIO()
                with mock.patch.dict(globals(), ROOT=root, PROFILE=root / ".config/rust-tests.toml"), \
                        mock.patch.object(sys, "argv", [__file__, "--base", "HEAD"]), \
                        mock.patch.dict(globals(), git_output=git_read), \
                        mock.patch.object(subprocess, "check_output", side_effect=output), \
                        contextlib.redirect_stdout(printed), contextlib.redirect_stderr(errors):
                    self.assertNotEqual(main(), 0)
                self.assertEqual(calls, ["metadata"])
                self.assertEqual(printed.getvalue(), "")
                self.assertGreaterEqual(axis_reads, diagnostic_read)
                self.assertIn("Source changed during affected planning", errors.getvalue())

    def test_receipt_identity_includes_untracked_bytes_and_boundaries(self) -> None:
        # Q9/Q10: independently alter each framed component, including index
        # and the two deltas even when their combined working patch is empty.
        parts = [b"revision", b"index", b"staged", b"working"]
        expected = source_digest(*parts, {"a": b"bc"})
        # S1 golden from the original A framing, before moving its one owner.
        self.assertEqual(expected, "dbb6396b4173947748c9a8d811481cbc8fd6efc450fb52a3d4d6f9549e2bcc59")
        for axis in range(4):
            changed = [*parts]
            changed[axis] = b"other"
            self.assertNotEqual(expected, source_digest(*changed, {"a": b"bc"}))
        for files in ({"a": b"different"}, {"ab": b"c"}):
            self.assertNotEqual(expected, source_digest(*parts, files))
        self.assertEqual(source_digest(*parts, {"b": b"2", "a": b"1"}), source_digest(*parts, {"a": b"1", "b": b"2"}))

    def test_shared_reader_refusal_preserves_its_status_before_metadata(self) -> None:
        # S10: the canonical root is real, but its shared Git-read boundary
        # refuses with23/75. Main preserves that original status and executes
        # zero metadata/nextest calls, never interpreting missing facts as empty.
        for status in (23, 75):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                root, _ = self.fixture_repository(directory)
                calls = []
                original = subprocess.check_output
                def output(command, **kwargs):
                    if command[0] == "cargo":
                        calls.append(command[1])
                        raise AssertionError("Cargo must not run after source refusal")
                    return original(command, **kwargs)
                errors = io.StringIO()
                with mock.patch.dict(globals(), ROOT=root), \
                        mock.patch.object(sys, "argv", [__file__, "--base", "HEAD", "--run"]), \
                        mock.patch.object(subprocess, "check_output", side_effect=output), \
                        mock.patch.object(formal_sources, "_git_output", side_effect=subprocess.CalledProcessError(status, ["git", "source-read"])), \
                        contextlib.redirect_stderr(errors):
                    self.assertEqual(main(), status)
                self.assertEqual(calls, [])
                self.assertIn(f"status {status}", errors.getvalue())
    def test_shared_profile_names_declared_workspace_features(self) -> None:
        manifests = {tomllib.loads(path.read_text(encoding="utf-8"))["package"]["name"]: path for path in ROOT.glob("crates/*/*/Cargo.toml")}
        for feature in workspace_features().split(","):
            package, name = feature.split("/", 1)
            self.assertIn(package, manifests)
            self.assertIn(name, tomllib.loads(manifests[package].read_text(encoding="utf-8")).get("features", {}))

    def test_ownership_reverse_consumers_and_global_fallback(self) -> None:
        # A1 leaf => leaf+consumer+composition; A2 unrelated owner excluded.
        # A3 manifests/CI/unowned/deleted package => full workspace. A4 only
        # docs/no changes => explicit no-work, never an invented green receipt.
        names = ("kernel", "application", "cli", "other")
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(ROOT / "crates" / n / "Cargo.toml")} for n in names],
            "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in deps]} for n, deps in (("kernel", ()), ("application", ("kernel",)), ("cli", ("application",)), ("other", ()))]},
        }
        for paths, expected in ((["crates/kernel/src/lib.rs"], ["application", "cli", "kernel"]), (["crates/cli/tests/new.rs"], ["cli"]), (["docs/design.md"], []), ([], [])):
            self.assertEqual(affected_packages(metadata, paths)[0], expected)
        for path in ("Cargo.lock", "Cargo.toml", "crates/other/Cargo.toml", ".config/rust-tests.toml", "scripts/ci/check-rust.sh", "crates/deleted/src/lib.rs", "e2e/scenario.ts"):
            self.assertEqual(affected_packages(metadata, [path])[0], sorted(names))
        metadata["resolve"] = None
        with self.assertRaises(ValueError):
            affected_packages(metadata, ["crates/kernel/src/lib.rs"])


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--features", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--source-digest", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    if args.features or args.self_test or args.source_digest:
        if args.base or args.run or sum((args.features, args.self_test, args.source_digest)) != 1:
            parser.error("--features, --source-digest and --self-test are standalone modes")
        if args.source_digest:
            print(source_snapshot()[0])
            return 0
        if args.features:
            print(workspace_features())
            return 0
        return int(not unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PlanTests)).wasSuccessful())
    if not args.base:
        parser.error("--base is required for an affected plan")
    try:
        # Bind the selection before reading base, feature profile or metadata.
        # The later equality check must not bless a new subject with an old plan.
        fingerprint, patch = source_snapshot()
        base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
        changed = changed_sources(base)
        features = workspace_features()
        environment = dict(os.environ)
        environment.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
        environment["CARGO_BUILD_BUILD_DIR"] = environment["CARGO_TARGET_DIR"]
        metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1", "--features", features], cwd=ROOT, env=environment))
        selected, reason = affected_packages(metadata, changed, ROOT)
        command = ["cargo", "nextest", "run", "--workspace", "--locked", "--features", features]
        if selected:
            command += ["-E", " | ".join(f"package(={name})" for name in selected)]
        plan = {"schema_version": 1, "kind": "local-affected", "revision": git_output("rev-parse", "HEAD").decode().strip(), "base": base, "changed": changed, "selected_packages": selected, "reason": reason, "command": command if selected else [], "full_gate": False,
                "source_kind": "working-tree", "source_digest": fingerprint, "index_tree": git_output("write-tree").decode().strip(),
                "untracked_files": {name: hashlib.sha256(data).hexdigest() for name, data in untracked_sources().items()}}
        # Every diagnostic fact above is read before this final planning fence,
        # including no-run/no-work plans; none may mix a later axis with digest.
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed during affected planning; no execution started")
        print(json.dumps(plan, indent=2), flush=True)
        if not args.run or not selected:
            return 0
        directory = Path(environment["CARGO_TARGET_DIR"]) / "test-timings"
        directory.mkdir(parents=True, exist_ok=True)
        receipt = directory / f"affected-{time.time_ns()}.json"
        receipt.with_suffix(".patch").write_bytes(patch)
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed before affected execution; no execution started")
        started = time.monotonic()
        status = subprocess.run(command, cwd=ROOT, env=environment).returncode
        if status < 0:
            status = 128 - status
        try:
            unchanged = source_snapshot()[0] == fingerprint
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            print(f"Cannot verify affected execution subject: {error}", file=sys.stderr)
            unchanged = False
        if not unchanged:
            print("Source changed during affected execution; result cannot qualify this tree", file=sys.stderr)
            status = status or 1
        plan.update(exit_code=status, source_unchanged=unchanged, duration_seconds=time.monotonic() - started)
        try:
            receipt.write_text(json.dumps(plan, indent=2), encoding="utf-8")
        except OSError as error:
            print(f"Cannot retain affected execution evidence: {error}", file=sys.stderr)
            return status or 1
        print(f"Affected execution evidence: {receipt}")
        return status
    except subprocess.CalledProcessError as error:
        # Planning refusal is the command's refusal, not Python's generic 1.
        # In particular metadata75 must stop before nextest or PASS evidence.
        status = error.returncode if error.returncode >= 0 else 128 - error.returncode
        if error.output:
            print(os.fsdecode(error.output), file=sys.stderr, end="")
        print(f"Affected planning command failed (status {status}); no execution started: {error.cmd}", file=sys.stderr)
        return status
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot establish affected working subject or plan: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
