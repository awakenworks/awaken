#!/usr/bin/env python3
"""Derive local affected tests from the actual Cargo graph; never bless a full gate."""
from __future__ import annotations

import argparse
from collections import Counter, deque
import contextlib
import hashlib
import io
import json
import os
import re
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
import crate_test
import generate_hooks
from _crate_boundary_workspace import crate_manifests

ROOT = Path(__file__).resolve().parents[2]
PROFILE = ROOT / ".config/rust-tests.toml"
INPUT_GLOBS = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/**', 'fuzz/**', 'apps/**', 'tests/**', 'xtask/**', 'e2e/fixtures/**', '.cargo/**', '.config/**', 'scripts/ci/**', 'lefthook.yml', 'scripts/ci/test_inventory.txt', 'scripts/ci/modularity_baseline.json')


def workspace_features() -> str:
    features, _ = crate_test.load_profile(PROFILE)
    return ",".join(features)


def affected_packages(metadata: dict, changed: list[str], root: Path = ROOT) -> tuple[list[str], str]:
    packages = {p["id"]: p for p in crate_test._members(metadata).values()}
    members = set(packages)
    nodes = metadata["resolve"]["nodes"]
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


# Scope decision table for the separate-graph gate steps (check-rust.sh and the
# public API gate). P1 no changed path is a declared Rust gate input (this
# planner's INPUT_GLOBS and the SDK consumer checker's, matched the way the
# rendered hook globs match) -> "none". P2 every input lies in a member crate ->
# "packages": the changed owners and their transitive consumers. P3 a manifest,
# lockfile, toolchain, Cargo config, gate script or other unowned input ->
# "whole": every member stays selected, exactly as before scoping existed.
def gate_inputs() -> tuple[str, ...]:
    declared = generate_hooks.declared_input_globs(ROOT / "scripts/ci")
    return tuple(dict.fromkeys((*INPUT_GLOBS, *declared["check_sdk_consumer.py"])))


def gate_scope(metadata: dict, changed: list[str], root: Path = ROOT,
               inputs: tuple[str, ...] | None = None) -> tuple[str, list[str], str]:
    """Classify a change as "none", "packages" or "whole" for a scoped gate step."""
    inputs = gate_inputs() if inputs is None else inputs
    relevant = [path for path in changed if any(generate_hooks.glob_matches(glob, path) for glob in inputs)]
    if not relevant:
        return "none", [], "no declared Rust gate input changed"
    selected, reason = affected_packages(metadata, relevant, root)
    if reason == "changed owners and transitive consumers, including test dependencies":
        return "packages", selected, reason
    return "whole", selected, reason



# These are data inputs to whole-tree static checks, not executable inputs.
# Keep INPUT_GLOBS and gate_scope conservative: public API and push qualification
# retain their existing policy. Only the explicit local affected-test planner
# may separate these inputs, and it must run the complete static lane first.
# Exact data inputs are derived from this owner's input declaration. The
# content classifier below still rejects every path except its two schemas.
STATIC_EXECUTION_INPUTS = frozenset(
    path for path in INPUT_GLOBS
    if path.endswith((".txt", ".json")) and not any(char in path for char in "*?[")
)


def static_metadata_tightens(path: str, before: bytes, after: bytes) -> bool:
    """Recognize only pointwise stronger floors/ceilings under the same schema."""
    try:
        old_text, new_text = before.decode("utf-8"), after.decode("utf-8")
        if path == "scripts/ci/test_inventory.txt":
            old, new = (Counter(line for line in text.splitlines() if line)
                        for text in (old_text, new_text))
            names = set(old) | set(new)
            return (all(re.fullmatch(r"\$?[^\W\d]\w*", name) for name in names)
                    and all(new[name] >= count for name, count in old.items()))
        if path != "scripts/ci/modularity_baseline.json":
            return False

        def unique(pairs):
            value = {}
            for key, item in pairs:
                if key in value:
                    raise ValueError("duplicate metadata key")
                value[key] = item
            return value

        old, new = (json.loads(text, object_pairs_hook=unique)
                    for text in (old_text, new_text))
        numeric = {"super-glob", "module-cycle", "long-function", "crate-line-ceiling"}
        structured = {"impl-spread": {"files", "methods"}, "crate-budget": {"deps"}}
        frozen = {"unreachable-pub-pending", "growth_reasons", "$comment"}
        if (not isinstance(old, dict) or not isinstance(new, dict)
                or old.keys() != new.keys()
                or set(old) - numeric - structured.keys() - frozen):
            return False

        def ceiling(a, b):
            return type(a) is int and type(b) is int and 0 <= b <= a

        for rule in old:
            a, b = old[rule], new[rule]
            if rule in frozen:
                if json.dumps(a, sort_keys=True) != json.dumps(b, sort_keys=True):
                    return False
                continue
            if not isinstance(a, dict) or not isinstance(b, dict) or a.keys() != b.keys():
                return False
            for key in a:
                if rule in numeric:
                    if not ceiling(a[key], b[key]):
                        return False
                    continue
                previous, candidate = a[key], b[key]
                allowed = structured[rule]
                required = allowed
                if (not isinstance(previous, dict) or not isinstance(candidate, dict)
                        or previous.keys() != candidate.keys()
                        or set(previous) - allowed - {"reason"}
                        or not required <= previous.keys()
                        or not isinstance(previous.get("reason"), str)
                        or not previous["reason"].strip()
                        or previous["reason"] != candidate.get("reason")):
                    return False
                if any(not ceiling(previous[field], candidate[field])
                       for field in previous.keys() & allowed):
                    return False
        return True
    except (UnicodeError, ValueError, TypeError, RecursionError):
        return False


def rust_execution_inputs(base: str, changed: list[str], root: Path = ROOT) -> tuple[list[str], list[str]]:
    """Preserve base/index/WT axes; a cancelling or ambiguous edit stays global."""
    runtime, static = [], []
    for path in changed:
        admitted = False
        if path in STATIC_EXECUTION_INPUTS:
            try:
                read = lambda *args: formal_sources.git_output(root, *args)
                tree = read("ls-tree", "-z", base, "--", path).split(b"\0")
                index = read("ls-files", "--stage", "-z", "--", path).split(b"\0")
                tree, index = [item for item in tree if item], [item for item in index if item]
                target = root / path
                if (len(tree) == len(index) == 1
                        and tree[0].split(b"\t", 1)[0].split()[:2] == [b"100644", b"blob"]
                        and index[0].split(b"\t", 1)[0].split()[::2] == [b"100644", b"0"]
                        and not target.is_symlink()
                        and target.resolve().is_relative_to(root.resolve())):
                    original = read("show", base + ":" + path)
                    staged = formal_sources.file_bytes(root, path, True)
                    working = formal_sources.file_bytes(root, path, False)
                    if staged is not None and working is not None:
                        admitted = (static_metadata_tightens(path, original, staged)
                                    and static_metadata_tightens(path, staged, working))
            except (OSError, ValueError, IndexError, subprocess.CalledProcessError):
                pass  # Unknown inputs retain the original whole fallback.
        (static if admitted else runtime).append(path)
    return runtime, static


def git_output(*arguments: str) -> bytes:
    return formal_sources.git_output(ROOT, *arguments)


def source_digest(revision: bytes, index: bytes, staged: bytes, working: bytes, untracked: dict[str, bytes]) -> str:
    return formal_sources.framed_digest((revision, index, staged, working, *formal_sources.untracked_parts(untracked)))


def working_subject() -> tuple[str, dict]:
    """Project one stable working capture into its original digest and facts.

    Index and WT deltas are independent: they may cancel in HEAD->WT. The
    returned diagnostic patch remains HEAD->WT; its digest also binds the index,
    both deltas and untracked bytes. A local run never qualifies a staged commit.
    """
    facts = formal_sources.source_facts(ROOT, with_index_tree=True)
    return source_digest(facts["revision"], facts["index_tree"], facts["staged"], facts["working"], facts["untracked"]), facts


def source_snapshot() -> tuple[str, bytes]:
    """Retain the original digest/patch API over the same working capture."""
    digest, facts = working_subject()
    return digest, facts["patch"]


def changed_sources(base: str) -> list[str]:
    """Union index, WT and untracked causes; rename deletion/addition both count."""
    return sorted(set(
        os.fsdecode(git_output("diff", "--cached", "--no-renames", "--name-only", "-z", base, "--")).split("\0")
        + os.fsdecode(git_output("diff", "--no-renames", "--name-only", "-z", "--")).split("\0")
        + os.fsdecode(git_output("ls-files", "--others", "--exclude-standard", "-z")).split("\0")
    ) - {""})


def compile_scope(metadata: dict, changed: list[str], original: list[str], root: Path = ROOT, target_reason: str = "") -> tuple[list[str], list[str], str]:
    """Keep every Cargo peer that can contribute to a retained dependency unit.

    This deliberately uses package overlap, not a feature solver. If an excluded
    root shares no resolved package with the retained closure, its normal/dev/
    build requests cannot change any retained unit's features. Ambiguous flags,
    global inputs, no changed source or a whole connected component stay whole.
    """
    selected, reason = affected_packages(metadata, changed, root)
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
    if target_reason:
        return original, sorted(packages), "whole: " + target_reason
    if not selected or len(selected) == len(packages) or reason != "changed owners and transitive consumers, including test dependencies":
        return original, sorted(packages), "whole: " + reason
    if original[:4] != ["cargo", "check", "--workspace", "--all-targets"]:
        return original, sorted(packages), "whole: unsupported compile command shape"
    flags = original[4:]
    feature_owners: set[str] = set()
    index = 0
    while index < len(flags):
        argument = flags[index]
        if argument in ("--locked", "--message-format=json", "--timings"):
            index += 1
        elif argument == "--features" and index + 1 < len(flags):
            for feature in flags[index + 1].replace(" ", ",").split(","):
                owner, separator, name = feature.partition("/")
                if not separator or not name or owner not in packages:
                    return original, sorted(packages), "whole: unowned compile feature"
                feature_owners.add(owner)
            index += 2
        else:
            return original, sorted(packages), "whole: unsupported target/profile/config flag"
    names = crate_test.peer_packages(metadata, {*selected, *feature_owners})
    if len(names) == len(packages):
        return original, names, "whole: shared-unit peer closure reaches the workspace"
    return ["cargo", "check", *(part for name in names for part in ("-p", name)), "--all-targets", *flags], names, "affected owners and all potentially shared-unit contributing peers"


class PlanTests(unittest.TestCase):
    def test_native_command_fixture_honors_compiler_and_wrapper(self) -> None:
        from _native_test_launcher import compiler_command
        # The configured wrapper receives the real compiler as one OS argument;
        # absent/empty wrappers keep direct execution, including paths with spaces.
        compiler = "compiler with spaces/rustc"
        wrapper = "wrapper with spaces/cache"
        for configured, expected in [(None, [compiler]), ("", [compiler]),
                                      (wrapper, [wrapper, compiler])]:
            with self.subTest(wrapper=configured):
                environment = {"RUSTC": compiler}
                if configured is not None:
                    environment["RUSTC_WRAPPER"] = configured
                self.assertEqual(compiler_command(environment), expected)

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            return
        from _native_test_launcher import native_fixture_launcher
        cls.fixture_launcher = native_fixture_launcher()

    def setUp(self) -> None:
        # Reuse the existing Git-environment authority. A hook exports its own
        # repository/index; no throwaway fixture may initialize or configure it.
        from formal_sources import detached_git_environment
        self.detached_environment = detached_git_environment
        # Fixtures author their compile graph; caller linker/target overrides
        # are independent inputs exercised by native_graph_reason's own table.
        values = {key: value for key, value in detached_git_environment().items()
                  if key not in crate_test.COMPILE_ENVIRONMENT_OVERRIDES}
        environment = mock.patch.dict(os.environ, values, clear=True)
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

    def test_static_metadata_admits_only_pointwise_stronger_original_shapes(self) -> None:
        self.assertEqual(STATIC_EXECUTION_INPUTS, frozenset({
            "scripts/ci/test_inventory.txt", "scripts/ci/modularity_baseline.json",
        }))
        floor = "scripts/ci/test_inventory.txt"
        for before, after, expected in [
            (b"kept\nkept\n", b"kept\nkept\nnew\n", True),
            (b"kept\nkept\n", b"kept\nnew\nnew\n", False),
            (b"kept\n", b"kept\n", True),
            (b"kept\n", b"replacement\n", False),
            (b"kept\n", b"kept\n invalid \n", False),
            (b"$name\n", b"$name\nother\n", True),
        ]:
            with self.subTest(before=before, after=after):
                self.assertEqual(static_metadata_tightens(floor, before, after), expected)
        path = "scripts/ci/modularity_baseline.json"
        old = {"impl-spread": {"crate::Owner": {"files": 4, "methods": 58, "reason": "split owner"}},
               "super-glob": {"crate": 3}, "unreachable-pub-pending": ["pending"],
               "crate-line-ceiling": {"crate": 6000}, "crate-budget": {"crate": {"deps": 3, "reason": "review"}}}
        encode = lambda value: json.dumps(value).encode()
        new = json.loads(json.dumps(old)); new["impl-spread"]["crate::Owner"]["methods"] = 57
        new["crate-line-ceiling"]["crate"] = 5500; new["crate-budget"]["crate"]["deps"] = 2
        self.assertTrue(static_metadata_tightens(path, encode(old), encode(new)))
        for fault in ("increase", "boolean", "negative", "float", "reason", "remove", "new-key", "pending", "unknown",
                      "ceiling-raise", "ceiling-new", "budget-lines", "budget-deps"):
            candidate = json.loads(json.dumps(new))
            row = candidate["impl-spread"]["crate::Owner"]
            if fault == "increase": candidate["super-glob"]["crate"] = 4
            elif fault == "boolean": row["methods"] = True
            elif fault == "negative": row["methods"] = -1
            elif fault == "float": row["methods"] = 57.0
            elif fault == "reason": row["reason"] = "another review"
            elif fault == "remove": del candidate["impl-spread"]["crate::Owner"]
            elif fault == "new-key": candidate["super-glob"]["other"] = 0
            elif fault == "pending": candidate["unreachable-pub-pending"] = []
            elif fault == "ceiling-raise": candidate["crate-line-ceiling"]["crate"] = 6001
            elif fault == "ceiling-new": candidate["crate-line-ceiling"]["other"] = 1000
            elif fault == "budget-lines": candidate["crate-budget"]["crate"]["lines"] = 1
            elif fault == "budget-deps": candidate["crate-budget"]["crate"]["deps"] = 4
            else: candidate["future-rule"] = {}
            with self.subTest(fault=fault):
                self.assertFalse(static_metadata_tightens(path, encode(old), encode(candidate)))
        self.assertFalse(static_metadata_tightens(path, b'{"super-glob":{},"super-glob":{}}', b'{}'))
        self.assertFalse(static_metadata_tightens(path, b"not-json", encode(new)))
        self.assertFalse(static_metadata_tightens("scripts/ci/unknown.json", b"{}", b"{}"))
        self.assertFalse(static_metadata_tightens(path, b'{"growth_reasons":{"opaque":1}}',
                                                  b'{"growth_reasons":{"opaque":true}}'))
        self.assertFalse(static_metadata_tightens(path, b"[" * 1500, b"{}"))

    def test_execution_scope_separates_static_plans_without_executing_or_accepting_races(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            name = "scripts/ci/test_inventory.txt"
            file = root / name
            file.write_bytes(b"kept\n")
            self.git(root, "add", name)
            self.git(root, "commit", "--quiet", "-m", "floor")
            file.write_bytes(b"kept\nadded\n")
            original_output = subprocess.check_output
            original_scope = gate_scope
            for flag, race in (("--gate-scope", False), ("--execution-scope", False),
                               ("--execution-scope", True)):
                with self.subTest(flag=flag, race=race):
                    printed, errors = io.StringIO(), io.StringIO()
                    def output(command, **kwargs):
                        if Path(command[0]).stem.lower() == "cargo":
                            if race:
                                file.write_bytes(b"kept\nadded\nlate\n")
                            return json.dumps(metadata).encode()
                        return original_output(command, **kwargs)
                    with mock.patch.dict(globals(), ROOT=root), \
                            mock.patch.object(sys, "argv", [__file__, flag, "--base", "HEAD"]), \
                            mock.patch.dict(globals(), gate_scope=lambda m, paths: original_scope(m, paths, root, ("scripts/ci/**", "crates/**"))), \
                            mock.patch.object(subprocess, "check_output", side_effect=output), \
                            mock.patch.object(crate_test, "execute_commands") as execution, \
                            contextlib.redirect_stdout(printed), contextlib.redirect_stderr(errors):
                        status = main()
                    execution.assert_not_called()
                    if race:
                        self.assertNotEqual(status, 0)
                        self.assertEqual(printed.getvalue(), "")
                        self.assertIn("Source changed during execution-scope planning", errors.getvalue())
                    elif flag == "--gate-scope":
                        self.assertEqual(status, 0, errors.getvalue())
                        self.assertTrue(printed.getvalue().startswith("scope=whole\n"))
                    else:
                        self.assertEqual(status, 0, errors.getvalue())
                        plan = json.loads(printed.getvalue())
                        self.assertEqual((plan["scope"], plan["packages"], plan["static_inputs"]),
                                         ("none", [], [name]))
                        self.assertEqual(plan["static_scope"], "whole")
                        self.assertFalse(plan["execution_performed"])
                        self.assertFalse(plan["full_gate"])
                        self.assertEqual(len(plan["source_digest"]), 64)

    def test_static_execution_partition_preserves_index_working_and_unknown_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            name = "scripts/ci/test_inventory.txt"
            file = root / name
            file.write_bytes(b"kept\n")
            self.git(root, "add", name)
            self.git(root, "commit", "--quiet", "-m", "floor")
            file.write_bytes(b"kept\nadded\n")
            self.git(root, "add", name)
            file.write_bytes(b"kept\n")
            # The index grew but WT weakened it. HEAD->WT equality must not hide it.
            self.assertEqual(rust_execution_inputs("HEAD", [name], root), ([name], []))
            file.write_bytes(b"kept\nadded\nmore\n")
            cause = "crates/kernel/src/lib.rs"
            self.assertEqual(rust_execution_inputs("HEAD", [name, cause], root), ([cause], [name]))
            declared = ("scripts/ci/**", "crates/**", "Cargo.lock", "examples/sdk-consumer/**")
            runtime, static = rust_execution_inputs("HEAD", [name, cause], root)
            self.assertEqual(gate_scope(metadata, runtime, root, declared)[0], "packages")
            self.assertEqual(gate_scope(metadata, [name, cause], root, declared)[0], "whole")
            for unknown in ("Cargo.lock", "scripts/ci/test_inventory_ledger.json",
                            "scripts/ci/check_test_inventory.py", "examples/sdk-consumer/src/main.rs"):
                runtime, _ = rust_execution_inputs("HEAD", [name, unknown], root)
                self.assertEqual(gate_scope(metadata, runtime, root, declared)[0], "whole")
            with mock.patch.object(Path, "is_symlink", return_value=True):
                self.assertEqual(rust_execution_inputs("HEAD", [name], root), ([name], []))
            self.assertEqual(rust_execution_inputs("HEAD", [name, "Cargo.lock"], root), (["Cargo.lock"], [name]))
            file.unlink()
            self.assertEqual(rust_execution_inputs("HEAD", [name], root), ([name], []))

    def test_local_static_lane_precedes_rust_and_failure_never_runs_rust(self) -> None:
        execute = crate_test.execute_commands
        for with_rust in (False, True):
            for static_status in (0, 23):
                with self.subTest(with_rust=with_rust, static_status=static_status), tempfile.TemporaryDirectory() as directory:
                    root, metadata = self.fixture_repository(directory)
                    name = "scripts/ci/test_inventory.txt"
                    (root / name).write_bytes(b"kept\n")
                    self.git(root, "add", name)
                    self.git(root, "commit", "--quiet", "-m", "floor")
                    (root / name).write_bytes(b"kept\nnew\n")
                    if with_rust:
                        (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                    calls = []
                    def run(command, **kwargs):
                        calls.append(command)
                        status = static_status if "--group" in command else 0
                        return subprocess.CompletedProcess(command, status)
                    def execute_plan(commands, environment, **kwargs):
                        return execute(commands, environment, run=run, **kwargs)
                    target = Path(directory) / "evidence"
                    with mock.patch.dict(globals(), ROOT=root, PROFILE=root / ".config/rust-tests.toml"), \
                            mock.patch.object(crate_test, "full_graph_metadata", return_value=metadata), \
                            mock.patch.object(crate_test, "execute_commands", side_effect=execute_plan), \
                            mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(target)), \
                            mock.patch.object(sys, "argv", [__file__, "--base", "HEAD", "--run"]), \
                            contextlib.redirect_stdout(io.StringIO()):
                        status = main()
                    self.assertEqual(status, static_status)
                    self.assertIn("--group", calls[0])
                    self.assertEqual(calls[0][-1], "static")
                    self.assertEqual(len(calls), 1 + int(with_rust and not static_status))
                    receipt = json.loads(next((target / "test-timings").glob("affected-*.json")).read_text())
                    self.assertEqual(receipt["static_inputs"], [name])
                    self.assertEqual(receipt["static_scope"], "whole")
                    self.assertFalse(receipt["full_gate"])
                    self.assertTrue(receipt["source_unchanged"])
                    self.assertEqual(receipt["selected_packages"], ["application", "cli", "kernel"] if with_rust else [])

    def test_gate_scope_follows_the_decision_table(self) -> None:
        # P1 no declared Rust gate input -> none; P2 member sources -> owners
        # plus transitive consumers; P3 a manifest, lockfile, gate script or an
        # unowned declared input (the SDK consumer workspace) -> whole.
        root = Path("/repository")
        names = ("kernel", "application", "cli", "other")
        edges = {"kernel": (), "application": ("kernel",), "cli": ("application",), "other": ()}
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(root / "crates" / n / "Cargo.toml")} for n in names],
            "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in edges[n]]} for n in names]},
        }
        inputs = ("Cargo.toml", "Cargo.lock", "crates/**", "examples/sdk-consumer/**", "scripts/ci/**", "lefthook.yml")
        rows = (
            ([], "none", []),
            (["docs/runtime.md", "README.md", "web/src/main.ts", "e2e/scenario.mjs"], "none", []),
            (["crates/kernel/src/lib.rs"], "packages", ["application", "cli", "kernel"]),
            (["crates/other/src/lib.rs", "docs/other.md"], "packages", ["other"]),
            (["Cargo.lock"], "whole", None),
            (["crates/kernel/Cargo.toml"], "whole", None),
            (["scripts/ci/check-rust.sh"], "whole", None),
            (["examples/sdk-consumer/tests/tasks.rs", "crates/other/src/lib.rs"], "whole", None),
        )
        for changed, kind, packages in rows:
            with self.subTest(changed=changed):
                actual_kind, actual_packages, reason = gate_scope(metadata, changed, root, inputs)
                self.assertEqual(actual_kind, kind, reason)
                if packages is not None:
                    self.assertEqual(actual_packages, packages)
        declared = gate_inputs()
        for path in ("crates/runtime/awaken-runtime/src/lib.rs", "Cargo.lock", "fuzz/Cargo.toml",
                     "examples/sdk-consumer/Cargo.toml", "scripts/ci/check-rust.sh", "lefthook.yml"):
            self.assertTrue(any(generate_hooks.glob_matches(glob, path) for glob in declared), path)

    def test_embedded_e2e_fixture_inputs_cannot_skip_rust_ownership(self) -> None:
        root = ROOT
        members = ("first", "second")
        metadata = {"workspace_members": list(members),
                    "packages": [{"id": name, "name": name, "manifest_path": str(root / "crates" / name / "Cargo.toml")} for name in members],
                    "resolve": {"nodes": [{"id": name, "deps": []} for name in members]}}
        for path in ("e2e/fixtures/acp_awaken_tool_bridge_fixture.mjs",
                     "e2e/fixtures/mcp_http_fixture_client.mjs", "e2e/fixtures/future_fixture.mjs"):
            with self.subTest(path=path):
                kind, selected, _ = gate_scope(metadata, [path], root)
                self.assertEqual(kind, "whole")
                self.assertEqual(selected, sorted(members))
        self.assertEqual(gate_scope(metadata, ["e2e/scenario.mjs"], root)[0], "none")

    def planner_cli(self, directory: str, root: Path, metadata: dict, *, metadata_status: int = 0,
                    execution_status: int = 0, mutate: str = "", broken_metadata: bool = False,
                    run: bool = True, check_command: list[str] | None = None) -> tuple[subprocess.CompletedProcess, list[str], list[dict]]:
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
with pathlib.Path(os.environ["W41_FIXTURE_ARGV"]).open("a") as out:
    out.write(json.dumps(sys.argv[1:]) + "\\n")
if command == "metadata":
    status = int(os.environ["W41_FIXTURE_METADATA_STATUS"])
    if status:
        print("fixture metadata refusal", file=sys.stderr)
        sys.exit(status)
    if os.environ["W41_FIXTURE_MUTATE"] == "metadata":
        pathlib.Path("crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\\n")
    if os.environ["W41_FIXTURE_MUTATE"] == "metadata-config":
        home = pathlib.Path(os.environ["CARGO_HOME"])
        home.mkdir(exist_ok=True)
        (home / "config.toml").write_text("[build]\\ntarget='private-value'\\n")
    print("{" if os.environ["W41_FIXTURE_BROKEN"] == "1" else pathlib.Path(os.environ["W41_FIXTURE_METADATA"]).read_text())
elif command in ("nextest", "check"):
    if os.environ["W41_FIXTURE_MUTATE"] == "execution":
        pathlib.Path("crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\\n")
    if os.environ["W41_FIXTURE_MUTATE"] == "execution-config":
        home = pathlib.Path(os.environ["CARGO_HOME"])
        home.mkdir(exist_ok=True)
        (home / "config.toml").write_text("[build]\\ntarget='private-value'\\n")
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
            shutil.copyfile(self.fixture_launcher, tools / "cargo.exe")
        else:
            cargo = tools / "cargo"
            cargo.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{shim}" "$@"\n', encoding="utf-8")
            cargo.chmod(0o755)
        target = outer / "target"
        environment = dict(self.detached_environment(), PATH=str(tools) + os.pathsep + os.environ.get("PATH", ""),
                           CARGO_HOME=str(outer / "cargo-home"),
                           CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                           W41_FIXTURE_METADATA=str(metadata_path), W41_FIXTURE_CALLS=str(calls_path),
                           W41_FIXTURE_ARGV=str(outer / "argv.jsonl"),
                           AWAKEN_FIXTURE_PROGRAM=sys.executable, AWAKEN_FIXTURE_SCRIPT=str(shim),
                           W41_FIXTURE_METADATA_STATUS=str(metadata_status), W41_FIXTURE_EXECUTION_STATUS=str(execution_status),
                           W41_FIXTURE_MUTATE=mutate, W41_FIXTURE_BROKEN=str(int(broken_metadata)))
        result = subprocess.run([sys.executable, "-B", str(root / "scripts/ci/rust_test_plan.py"), "--base", "HEAD", *(["--run"] if run else []),
                                 *(["--check-command", *check_command] if check_command is not None else [])],
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
                if Path(command[0]).stem.lower() == "cargo":
                    calls.append(command[1])
                    return json.dumps(metadata).encode()
                return original_output(command, **kwargs)
            def write(path, data):
                result = original_write(path, data)
                if path.suffix == ".patch":
                    (root / "crates/other/src/lib.rs").write_text("pub const VALUE: u8 = 3;\n")
                return result
            def execute(command, **kwargs):
                if Path(command[0]).stem.lower() == "cargo":
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
                if Path(command[0]).stem.lower() == "cargo":
                    calls.append(command[1])
                    return json.dumps(metadata).encode()
                return original_output(command, **kwargs)
            def execute(command, **kwargs):
                if Path(command[0]).stem.lower() == "cargo":
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
        # metadata now projects that same capture, with no second index/HEAD
        # authority. Cross a real index/HEAD amendment before capture2 (the
        # final planning fence) and refuse before printing any plan or effects.
        for axis in ("index", "rev-parse"):
            with self.subTest(axis=axis), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                original_subject, original_output = working_subject, subprocess.check_output
                axis_reads = 0
                diagnostic_read = 2
                calls = []
                def read_subject():
                    nonlocal axis_reads
                    axis_reads += 1
                    if axis_reads == diagnostic_read:
                        source = root / "crates/other/src/lib.rs"
                        source.write_text("pub const VALUE: u8 = 3;\n")
                        self.git(root, "add", str(source))
                        if axis == "rev-parse":
                            self.git(root, "commit", "--quiet", "-m", "late fixture revision")
                    return original_subject()
                def output(command, **kwargs):
                    if Path(command[0]).stem.lower() == "cargo":
                        calls.append(command[1])
                        return json.dumps(metadata).encode()
                    return original_output(command, **kwargs)
                printed, errors = io.StringIO(), io.StringIO()
                with mock.patch.dict(globals(), ROOT=root, PROFILE=root / ".config/rust-tests.toml"), \
                        mock.patch.object(sys, "argv", [__file__, "--base", "HEAD"]), \
                        mock.patch.dict(globals(), working_subject=read_subject), \
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
                    if Path(command[0]).stem.lower() == "cargo":
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
        manifests = {tomllib.loads(path.read_text(encoding="utf-8"))["package"]["name"]: path for path in crate_manifests(ROOT / "crates")}
        for feature in workspace_features().split(","):
            package, name = feature.split("/", 1)
            self.assertIn(package, manifests)
            self.assertIn(name, tomllib.loads(manifests[package].read_text(encoding="utf-8")).get("features", {}))

    def test_nested_product_directories_keep_reverse_consumers_and_global_fallback(self) -> None:
        # R1 a deeply nested SDK owner selects its Platform and app consumers;
        # R2 a Platform edit excludes unrelated SDK owners; R3 a removed or
        # unknown owner still requires the full graph rather than disappearing.
        locations = {
            "engine": "crates/sdk/runtime/engine",
            "session": "crates/platform/session/session",
            "server": "crates/apps/server",
            "other": "crates/sdk/support/other",
        }
        metadata = {
            "workspace_members": list(locations),
            "packages": [{"id": name, "name": name, "manifest_path": str(ROOT / path / "Cargo.toml")} for name, path in locations.items()],
            "resolve": {"nodes": [{"id": name, "deps": [{"pkg": dep} for dep in deps]} for name, deps in (("engine", ()), ("session", ("engine",)), ("server", ("session",)), ("other", ()))]},
        }
        self.assertEqual(affected_packages(metadata, [locations["engine"] + "/src/lib.rs"])[0], ["engine", "server", "session"])
        self.assertEqual(affected_packages(metadata, [locations["session"] + "/src/lib.rs"])[0], ["server", "session"])
        self.assertEqual(affected_packages(metadata, ["crates/platform/deleted/src/lib.rs"])[0], sorted(locations))

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

    def test_compile_peer_closure_preserves_dependency_kinds_and_package_ids(self) -> None:
        # B1/B2/B3/B4/B7 decision table: changed root + an excluded peer shares
        # a dependency unit -> retain that peer regardless of default/dev/build/
        # target/optional provenance. A peer connected only through a newly
        # retained peer is also retained (fixed point). A different package ID
        # with the same crate name, or a disjoint root, contributes no such unit.
        # This is potential-contribution closure, not node.features equivalence.
        original = ["cargo", "check", "--workspace", "--all-targets", "--features", "cli/pod", "--locked"]
        names = ("kernel", "peer", "second", "cli", "other")
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(ROOT / "crates" / n / "Cargo.toml")} for n in names]
                        + [{"id": n, "name": "shared", "manifest_path": "/registry/shared/Cargo.toml"} for n in ("shared@1", "shared@2", "bridge")],
            "resolve": {"nodes": [{"id": n, "deps": [{"name": "renamed", "pkg": d, "dep_kinds": [{"kind": kind, "target": "cfg(unix)"}]} for d in deps]}
                                     for n, deps, kind in (("kernel", ("shared@1",), None), ("peer", ("shared@1", "bridge"), "dev"),
                                                          ("second", ("bridge",), "build"), ("cli", (), None), ("other", ("shared@2",), None),
                                                          ("shared@1", (), None), ("shared@2", (), None), ("bridge", (), None))]},
        }
        command, selected, reason = compile_scope(metadata, ["crates/kernel/src/lib.rs"], original)
        self.assertEqual(selected, ["cli", "kernel", "peer", "second"])
        self.assertEqual(command, ["cargo", "check", "-p", "cli", "-p", "kernel", "-p", "peer", "-p", "second", "--all-targets", *original[4:]])
        self.assertIn("potentially", reason)
        self.assertNotIn("-E", command)
        # B12 diagnostic HTML timings change no target/feature selection and
        # survive projection verbatim. Unknown timing formats stay whole.
        timed, timed_selected, _ = compile_scope(metadata, ["crates/kernel/src/lib.rs"], [*original, '--timings'])
        self.assertEqual(timed, [*command, '--timings'])
        self.assertEqual(timed_selected, selected)
        unknown = [*original, '--timings=json']
        self.assertEqual(compile_scope(metadata, ["crates/kernel/src/lib.rs"], unknown)[0], unknown)
        for kind in (None, "dev", "build"):
            metadata["resolve"]["nodes"][1]["deps"][0]["dep_kinds"][0]["kind"] = kind
            self.assertEqual(compile_scope(metadata, ["crates/kernel/src/lib.rs"], original)[1], selected)
        metadata["resolve"]["nodes"][-1]["deps"] = [{"pkg": "shared@2"}]
        self.assertEqual(compile_scope(metadata, ["crates/kernel/src/lib.rs"], original)[0], original)

    def test_compile_whole_fallback_is_explicit_before_execution(self) -> None:
        # B5/B6/B8/B9: clean/manual, docs-only, global/config/hooks/fuzz,
        # manifests (owned too), deleted/unowned sources or an unsupported
        # target/profile/feature request -> original whole argv, never no-work
        # or an altered feature graph. Invalid metadata is refusal, not fallback.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            original = ["cargo", "check", "--workspace", "--all-targets", "--locked"]
            for paths in ([], ["docs/readme.md"], ["Cargo.lock"], [".cargo/config.toml"], [".config/rust-tests.toml"],
                          ["lefthook.yml"], ["scripts/ci/crate_test.py"], ["fuzz/fuzz_targets/new.rs"],
                          ["crates/kernel/Cargo.toml"], ["crates/deleted/src/lib.rs"],
                          ["docs/readme.md", "crates/kernel/Cargo.toml"], ["docs/readme.md", "lefthook.yml"]):
                with self.subTest(paths=paths):
                    command, selected, reason = compile_scope(metadata, paths, original, root)
                    self.assertEqual(command, original)
                    self.assertEqual(selected, ["application", "cli", "kernel", "other"])
                    self.assertTrue(reason.startswith("whole:"), reason)
            for flags in (["--target", "x86_64-unknown-linux-gnu"], ["--release"], ["--no-default-features"],
                          ["--features", "foreign/feature"], ["--features", "kernel-feature"], ["--features"]):
                command = [*original, *flags]
                self.assertEqual(compile_scope(metadata, ["crates/kernel/src/lib.rs"], command, root)[0], command)
            metadata["resolve"]["nodes"][0]["deps"] = [{"pkg": "missing-unit"}]
            with self.assertRaisesRegex(ValueError, "omits a dependency node"):
                compile_scope(metadata, ["crates/kernel/src/lib.rs"], original, root)

    def test_compile_cli_binds_original_actual_argv_and_working_subject(self) -> None:
        # B10/B11: actual CLI reads the same index+WT+untracked identity and
        # authored last-step features; its subprocess argv equals its ordinary
        # local-affected-check receipt. The old nextest -E mode remains separate.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.fixture_repository(directory)
            source = root / "crates/kernel/src/lib.rs"
            source.write_text("pub const VALUE: u8 = 2;\n")
            self.git(root, "add", str(source))
            source.write_text("pub const VALUE: u8 = 1;\n")
            original = ["cargo", "check", "--workspace", "--all-targets", "--features", "cli/pod", "--locked"]
            result, calls, receipts = self.planner_cli(directory, root, metadata, check_command=original)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, ["metadata", "check"])
            receipt = receipts[0]
            self.assertEqual(receipt["kind"], "local-affected-check")
            self.assertEqual(receipt["original_command"], original)
            self.assertEqual(receipt["selected_packages"], ["application", "cli", "kernel"])
            actual = [json.loads(line) for line in (Path(directory) / "argv.jsonl").read_text().splitlines()]
            self.assertEqual(actual[-1], receipt["command"][1:])
            self.assertEqual(receipt["source_kind"], "working-tree")
            self.assertTrue(receipt["source_unchanged"])
            self.assertFalse(receipt["full_gate"])
            self.assertIn("--all-targets", actual[-1])
            self.assertIn("cli/pod", actual[-1])
            self.assertNotIn("-E", actual[-1])

    def test_compile_cli_refusal_source_change_and_execution_failure_never_retry(self) -> None:
        # B12/B13 decision table at the existing process boundary:
        # metadata75/23 -> exact status, no check/receipt; source mutation in
        # metadata -> refusal before check; check75/23 -> exact status and one
        # failed ordinary receipt, never retry whole; check mutates source ->
        # refusal even if its status was0. No row changes profiles or admission.
        original = ["cargo", "check", "--workspace", "--all-targets", "--locked"]
        for metadata_status, execution_status, mutate, expected, calls_expected in (
            (75, 0, "", 75, ["metadata"]), (23, 0, "", 23, ["metadata"]),
            (0, 0, "metadata", 1, ["metadata"]), (0, 75, "", 75, ["metadata", "check"]),
            (0, 23, "", 23, ["metadata", "check"]), (0, 0, "execution", 1, ["metadata", "check"]),
        ):
            with self.subTest(metadata=metadata_status, execution=execution_status, mutate=mutate), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                result, calls, receipts = self.planner_cli(directory, root, metadata, metadata_status=metadata_status,
                                                         execution_status=execution_status, mutate=mutate, check_command=original)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(calls, calls_expected)
                self.assertEqual(len(receipts), int("check" in calls))
                if receipts:
                    self.assertEqual(receipts[0]["exit_code"], expected)
                    self.assertEqual(receipts[0]["source_unchanged"], mutate != "execution")

    def test_compile_cli_config_appearance_refuses_without_claiming_source_change(self) -> None:
        # B8/B13: private CARGO_HOME starts with no config; the metadata/check
        # boundary creates one outside the repo. Planning appearance -> zero
        # compile; execution appearance -> failed receipt even if check0. Repo
        # source_unchanged remains its actual Git fact; do not call a contextual
        # failure a source change, print private values, or retry whole.
        original = ["cargo", "check", "--workspace", "--all-targets", "--locked"]
        for mutate, expected_calls in (("metadata-config", ["metadata"]), ("execution-config", ["metadata", "check"])):
            with self.subTest(mutate=mutate), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.fixture_repository(directory)
                (root / "crates/kernel/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
                result, calls, receipts = self.planner_cli(directory, root, metadata, mutate=mutate, check_command=original)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(calls, expected_calls)
                self.assertIn("Compile target admission changed", result.stderr)
                self.assertNotIn("private-value", result.stderr)
                if receipts:
                    self.assertTrue(receipts[0]["source_unchanged"])
                    self.assertEqual(receipts[0]["exit_code"], 1)


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--features", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--source-digest", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--check-command", nargs=argparse.REMAINDER)
    scope_modes = parser.add_mutually_exclusive_group()
    scope_modes.add_argument("--gate-scope", action="store_true")
    scope_modes.add_argument("--execution-scope", action="store_true")
    args = parser.parse_args()
    if args.gate_scope or args.execution_scope:
        if not args.base or args.run or args.check_command is not None or args.features or args.self_test or args.source_digest:
            parser.error(("--execution-scope" if args.execution_scope else "--gate-scope") + " takes only --base")
        try:
            base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
            subject = source_snapshot()[0] if args.execution_scope else None
            changed = changed_sources(base)
            environment = crate_test.environment(ROOT)
            # Every feature and platform: an edge that exists in any graph a
            # scoped step compiles must reach the consumer set.
            metadata = json.loads(subprocess.check_output(crate_test.native_command(
                ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"], environment),
                cwd=ROOT, env=environment))
            runtime_inputs, static_inputs = rust_execution_inputs(base, changed, ROOT) if args.execution_scope else (changed, [])
            kind, packages, reason = gate_scope(metadata, runtime_inputs)
            if args.execution_scope and source_snapshot()[0] != subject:
                raise ValueError("Source changed during execution-scope planning")
        except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
            print(f"Cannot establish the gate scope: {error}", file=sys.stderr)
            return 1
        if args.execution_scope:
            print(json.dumps(dict(scope=kind, packages=packages, reason=reason,
                                  static_scope="whole", static_inputs=static_inputs,
                                  source_digest=subject, full_gate=False,
                                  execution_performed=False), indent=2))
            return 0
        print(f"scope={kind}")
        print(f"reason={reason}")
        for package in packages:
            print(package)
        return 0
    if args.features or args.self_test or args.source_digest:
        if args.base or args.run or args.check_command is not None or sum((args.features, args.self_test, args.source_digest)) != 1:
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
        fingerprint, facts = working_subject()
        base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
        changed = changed_sources(base)
        if args.check_command is not None and args.check_command[:2] != ["cargo", "check"]:
            parser.error("--check-command requires the original cargo check argv")
        original = args.check_command
        if original is None:
            feature_names, companions = crate_test.load_profile(PROFILE)
            features = ",".join(feature_names)
        else:
            features = ",".join(original[i + 1] for i, value in enumerate(original[:-1]) if value == "--features")
        environment = crate_test.environment(ROOT)
        target_reason = crate_test.native_graph_reason(ROOT, environment) if original is not None else ""
        metadata = crate_test.full_graph_metadata(features.split(",") if features else [], root=ROOT,
                                                  environment=environment, host_only=original is not None and not target_reason)
        if original is not None:
            command, selected, reason = compile_scope(metadata, changed, original, ROOT, target_reason)
            commands = [command]
        else:
            runtime_inputs, static_inputs = rust_execution_inputs(base, changed, ROOT)
            selected, reason = affected_packages(metadata, runtime_inputs, ROOT)
            errors = crate_test.companion_errors(metadata, companions, ROOT)
            if errors:
                raise crate_test.PlanError("; ".join(errors))
            commands = crate_test.plan_packages(metadata, feature_names, companions, selected, []) if selected else []
            if static_inputs:
                commands.insert(0, [sys.executable, "scripts/ci/_native_shell.py",
                                    "scripts/ci/check-all.sh", "--group", "static"])
            command = commands[-1] if commands else []
        plan = {"schema_version": 1, "kind": "local-affected", "revision": facts["revision"].decode().strip(), "base": base, "changed": changed, "selected_packages": selected, "reason": reason, "command": command if selected else [], "full_gate": False,
                "source_kind": "working-tree", "source_digest": fingerprint, "index_tree": facts["index_tree"].decode().strip(),
                "untracked_files": {name: hashlib.sha256(data).hexdigest() for name, data in facts["untracked"].items()}}
        if original is not None:
            plan.update(kind="local-affected-check", original_command=original)
        plan["commands"] = commands
        if original is None:
            plan.update(static_inputs=static_inputs, static_scope="whole" if static_inputs else None)
        # Metadata projects the first capture. This fresh final fence still
        # refuses any late source axis, including no-run/no-work plans.
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed during affected planning; no execution started")
        if original is not None and crate_test.native_graph_reason(ROOT, environment) != target_reason:
            raise ValueError("Compile target admission changed during planning; no execution started")
        print(json.dumps(plan, indent=2), flush=True)
        if not args.run or not commands:
            return 0
        directory = Path(environment["CARGO_TARGET_DIR"]) / "test-timings"
        directory.mkdir(parents=True, exist_ok=True)
        receipt = directory / f"affected-{time.time_ns()}.json"
        receipt.with_suffix(".patch").write_bytes(facts["patch"])
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed before affected execution; no execution started")
        if original is not None and crate_test.native_graph_reason(ROOT, environment) != target_reason:
            raise ValueError("Compile target admission changed before execution; no execution started")
        started = time.monotonic()
        def verify():
            if source_snapshot()[0] != fingerprint:
                raise ValueError("Source changed before or during affected execution")
            if original is not None and crate_test.native_graph_reason(ROOT, environment) != target_reason:
                raise ValueError("Compile target admission changed during execution")
        def record(rows):
            plan.update(status="running", rows=rows)
            receipt.write_text(json.dumps(plan, indent=2), encoding="utf-8")
        environment['AWAKEN_BUILD_SOURCE_DIGEST'] = fingerprint
        status, rows = crate_test.execute_commands(commands, environment, root=ROOT, verify=verify, record=record)
        try:
            unchanged = source_snapshot()[0] == fingerprint
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            print(f"Cannot verify affected execution subject: {error}", file=sys.stderr)
            unchanged = False
        if not unchanged:
            print("Source changed during affected execution; result cannot qualify this tree", file=sys.stderr)
            status = status or 1
        if original is not None and crate_test.native_graph_reason(ROOT, environment) != target_reason:
            print("Compile target admission changed during execution; result cannot qualify this context", file=sys.stderr)
            status = status or 1
        plan.update(status="passed" if status == 0 else "failed", rows=rows, exit_code=status,
                    source_unchanged=unchanged, duration_seconds=time.monotonic() - started)
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
    except (OSError, ValueError, KeyError, TypeError, crate_test.PlanError) as error:
        print(f"Cannot establish affected working subject or plan: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
