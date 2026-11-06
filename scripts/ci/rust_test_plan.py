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
# Rust build inputs (R): what Cargo reads to resolve and compile the graph.
# The gate's own scripts (G) are not declared here: generate_hooks.py derives
# them from what check-rust.sh actually runs, so a docs or unrelated CI-script
# change never starts the Rust gate, and a gate-script change runs only the
# steps that execute it (gate_plan below).
INPUT_GLOBS = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'deny.toml', '.cargo/**', '.config/**',
               'crates/**', 'fuzz/**', 'apps/**', 'tests/**', 'xtask/**', 'e2e/fixtures/**')
GATE = "check-rust.sh"
# The selector itself: a change here can alter what every other path selects,
# so it never narrows its own run. Their Python imports and the helpers the
# gate sources for every step count as the selector too.
SELECTOR = ("rust_test_plan.py", "crate_test.py", "generate_hooks.py", GATE)


def workspace_features() -> str:
    features, _ = crate_test.load_profile(PROFILE)
    return ",".join(features)


TEST_SUPPORT = "test-support"


def _enables_test_support(features: dict, feature: str) -> bool:
    pending, seen = [feature], set()
    while pending:
        current = pending.pop()
        if current == TEST_SUPPORT:
            return True
        if current in seen:
            continue
        seen.add(current)
        for value in features.get(current, []):
            if value.split("/")[-1] == TEST_SUPPORT:
                return True
            if value in features:
                pending.append(value)
    return False


def _implicit_features(manifest: dict) -> list[str]:
    """Optional dependencies that Cargo exposes as features (no `dep:` use)."""
    named = {value[4:] for values in manifest.get("features", {}).values()
             for value in values if value.startswith("dep:")}
    tables = [manifest.get("dependencies", {})]
    tables += [target.get("dependencies", {}) for target in manifest.get("target", {}).values()]
    return sorted({name for table in tables for name, value in table.items()
                   if isinstance(value, dict) and value.get("optional") and name not in named})


def production_selection(manifests: list[Path] | None = None) -> list[str]:
    """Cargo arguments for the one production lint over every product crate.

    Every crate except `layer = "tooling"` is selected with every feature except
    test-support (and any feature reaching it); tooling crates are excluded
    because enabling test-support is their job. The crate-boundary checker holds
    the converse statically (product_test_support_violations): no other crate
    enables test-support through a normal dependency, a default, another feature
    or a tooling dependency. With resolver 2, `--lib --bins` resolves no dev
    dependency, so this graph is the production graph of each product root.
    """
    excluded: list[str] = []
    features: list[str] = []
    for path in crate_manifests() if manifests is None else manifests:
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        name = manifest["package"]["name"]
        if manifest["package"].get("metadata", {}).get("awaken", {}).get("layer") == "tooling":
            excluded.append(name)
            continue
        table = manifest.get("features", {})
        for feature in sorted(set(table) | set(_implicit_features(manifest))):
            if feature != "default" and not _enables_test_support(table, feature):
                features.append(f"{name}/{feature}")
    return [f"--exclude={name}" for name in sorted(excluded)] + ["--features", ",".join(features)]


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
# public API gate). Three scopes, plus the gate scripts a change reaches:
# P1 no Rust build input (R) and no gate script (G) changed -> "none".
# P2 a selector (SELECTOR, its imports, a helper the gate sources) changed, or
#    any G file of a gate other than check-rust.sh -> "whole" (fail-closed).
# P3 a manifest, lockfile, toolchain, Cargo config or unowned R input -> "whole".
# P4 R inputs inside member crates -> "packages": changed owners and their
#    transitive consumers.
# P5 only G files changed -> "packages" with no package; the steps whose
#    command runs a changed G file (the `script=` lines) still run.
HOOK_MODES = ("--quick", "--push")


def gate_scripts(entry: str = GATE, modes: tuple[str, ...] = HOOK_MODES) -> set[str]:
    """G: every scripts/ci file the gate runs or reads in the given hook modes."""
    names = {entry, *generate_hooks.script_closure(entry)}
    if entry == GATE:
        names = {entry}
        for mode in modes:
            for name in generate_hooks.invoked_scripts(f"run: scripts/ci/{GATE} {mode}"):
                names |= {name, *generate_hooks.script_closure(name)}
    return {f"scripts/ci/{name}" for name in names}


def selector_scripts(entry: str = GATE) -> set[str]:
    names = set(SELECTOR)
    for name in SELECTOR:
        if name.endswith(".py"):
            names |= set(generate_hooks.local_imports(name))
    gate = (ROOT / "scripts/ci" / GATE).read_text(encoding="utf-8")
    names |= set(re.findall(r"(?m)^source scripts/ci/([A-Za-z0-9_.-]+)$", gate))
    if entry != GATE:
        names |= {entry, *generate_hooks.script_closure(entry)}
    return {f"scripts/ci/{name}" for name in names}


def gate_inputs(modes: tuple[str, ...] = HOOK_MODES) -> tuple[str, ...]:
    """R plus G: every path whose change can alter what the Rust gate runs."""
    return tuple(dict.fromkeys((*rust_inputs(), *sorted(gate_scripts(GATE, modes)))))


def rust_inputs() -> tuple[str, ...]:
    """R: this planner's declaration plus the out-of-workspace SDK consumer's."""
    declared = generate_hooks.input_globs_of(ROOT / "scripts/ci/check_sdk_consumer.py") or ()
    sdk = (glob for glob in declared if not glob.startswith("scripts/"))
    return tuple(dict.fromkeys((*INPUT_GLOBS, *sdk)))


def gate_plan(metadata: dict, changed: list[str], root: Path = ROOT, entry: str = GATE,
              modes: tuple[str, ...] = HOOK_MODES) -> tuple[str, list[str], str, list[str]]:
    """(scope, packages, reason, reached gate scripts) for one change set."""
    rust = rust_inputs()
    built = [path for path in changed if any(generate_hooks.glob_matches(glob, path) for glob in rust)]
    gate = gate_scripts(entry, modes)
    scripts = [path for path in changed if path in gate]
    if not built and not scripts:
        return "none", [], "no Rust build input or gate script changed", []
    everything = sorted(p["name"] for p in crate_test._members(metadata).values())
    selector = [path for path in scripts if path in selector_scripts(entry)]
    if selector or (entry != GATE and scripts):
        return "whole", everything, "gate selector changed: " + (selector or scripts)[0], []
    reached = sorted(name for name in (path.removeprefix("scripts/ci/") for path in gate)
                     if f"scripts/ci/{name}" in scripts
                     or set(scripts) & {f"scripts/ci/{item}" for item in generate_hooks.script_closure(name)})
    if not built:
        return "packages", [], "only gate scripts changed: " + ", ".join(scripts), reached
    selected, reason = affected_packages(metadata, built, root)
    if reason == "changed owners and transitive consumers, including test dependencies":
        return "packages", selected, reason, reached
    return "whole", selected, reason, reached


def gate_scope(metadata: dict, changed: list[str], root: Path = ROOT,
               inputs: tuple[str, ...] | None = None) -> tuple[str, list[str], str]:
    """Classify a change as "none", "packages" or "whole" for a scoped gate step.

    With ``inputs``, only that declared subset is classified by ownership (no
    gate-script rule); callers use it to ask about one external input set.
    """
    if inputs is None:
        kind, selected, reason, _ = gate_plan(metadata, changed, root)
        return kind, selected, reason
    relevant = [path for path in changed if any(generate_hooks.glob_matches(glob, path) for glob in inputs)]
    if not relevant:
        return "none", [], "no declared Rust gate input changed"
    selected, reason = affected_packages(metadata, relevant, root)
    if reason == "changed owners and transitive consumers, including test dependencies":
        return "packages", selected, reason
    return "whole", selected, reason


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


def explain(changed: list[str], entry: str = GATE, modes: tuple[str, ...] = HOOK_MODES) -> list[str]:
    """One line per changed path: why the gate counts it (or ignores it)."""
    rust = rust_inputs()
    gate, selector = gate_scripts(entry, modes), selector_scripts(entry)
    lines = []
    for path in changed:
        if path in selector:
            why = "gate selector -> whole gate"
        elif path in gate:
            why = "gate script -> the steps that run it"
        elif any(generate_hooks.glob_matches(glob, path) for glob in rust):
            why = "Rust build input -> owners and consumers (whole if unowned or a manifest)"
        else:
            why = "not a Rust gate input -> ignored"
        lines.append(f"explain: {path}: {why}")
    return lines


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
        # are independent inputs.
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
        for relative in ("scripts/ci/rust_test_plan.py",
                         *(f"scripts/ci/{name}" for name in generate_hooks.script_closure("rust_test_plan.py"))):
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

    def test_production_selection_leaves_out_tooling_and_test_support(self) -> None:
        # S1 a tooling crate -> --exclude, none of its features. S2 test-support,
        # a feature reaching it (directly or through another feature) and
        # `default` -> not selected. S3 every other declared feature and an
        # implicit optional-dependency feature -> selected; a `dep:` optional is
        # not a feature.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifests = []
            for name, text in {
                "kit": '[package]\nname = "kit"\n[package.metadata.awaken]\nlayer = "tooling"\n'
                       '[features]\nextra = []\n',
                "product": '[package]\nname = "product"\n[package.metadata.awaken]\nlayer = "infrastructure"\n'
                           '[dependencies]\nimplicit = { version = "1", optional = true }\n'
                           'hidden = { version = "1", optional = true }\n'
                           '[features]\ndefault = ["postgres"]\npostgres = ["dep:hidden"]\n'
                           'test-support = ["store/test-support"]\nfixtures = ["test-support"]\n'
                           'chained = ["fixtures"]\ndirect = ["store/test-support"]\n',
            }.items():
                path = root / name / "Cargo.toml"
                path.parent.mkdir()
                path.write_text(text, encoding="utf-8")
                manifests.append(path)
            self.assertEqual(production_selection(manifests),
                             ["--exclude=kit", "--features", "product/implicit,product/postgres"])

    def test_gate_scope_follows_the_decision_table(self) -> None:
        # P1 no R and no G input -> none; P4 member sources -> owners plus
        # transitive consumers; P3 a manifest, lockfile or unowned R input
        # (the SDK consumer workspace) -> whole. With an explicit input set the
        # ownership classification alone applies (external callers).
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
        for path in ("crates/sdk/runtime/awaken-runtime/src/lib.rs", "Cargo.lock", "fuzz/Cargo.toml",
                     "examples/sdk-consumer/Cargo.toml", "scripts/ci/check-rust.sh", "scripts/ci/check_sdk_consumer.py"):
            self.assertTrue(any(generate_hooks.glob_matches(glob, path) for glob in declared), path)
        quick = gate_inputs(("--quick",))
        for path in ("scripts/ci/check_formal.sh", "scripts/ci/test_formal_run_evidence.py", "lefthook.yml",
                     "docs/design/test-validation.md", "scripts/ci/test_inventory.txt"):
            self.assertFalse(any(generate_hooks.glob_matches(glob, path) for glob in quick), path)

    def test_gate_plan_separates_rust_inputs_gate_scripts_and_the_selector(self) -> None:
        # G1 a CI script the gate never runs -> none (rust-quick has nothing to
        # do); G2 a gate script that is not the selector -> packages with no
        # package, and the reached script names its steps; G3 the selector or
        # a helper the gate sources -> whole; G4 R plus G -> owners and the
        # reached script; G5 any script of another gate's closure -> whole.
        root = Path("/repository")
        names = ("kernel", "other")
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(root / "crates" / n / "Cargo.toml")} for n in names],
            "resolve": {"nodes": [{"id": n, "deps": []} for n in names]},
        }
        self.assertEqual(gate_plan(metadata, ["scripts/ci/check_formal.sh", "docs/a.md"], root,
                                   modes=("--quick",))[0], "none", "G1")
        self.assertEqual(gate_plan(metadata, ["scripts/ci/test_formal_run_evidence.py"], root)[0], "none", "G1")
        kind, packages, reason, reached = gate_plan(metadata, ["scripts/ci/check_dependency_sources.py"], root)
        self.assertEqual((kind, packages), ("packages", []), reason)
        self.assertIn("check_dependency_sources.py", reached, "G2")
        self.assertNotIn("check_rustfmt.py", reached, "G2 unrelated steps stay skipped")
        for selector in ("scripts/ci/rust_test_plan.py", "scripts/ci/crate_test.py", "scripts/ci/generate_hooks.py",
                         "scripts/ci/check-rust.sh", "scripts/ci/_cargo_target.sh", "scripts/ci/formal_sources.py"):
            with self.subTest(selector=selector):
                self.assertEqual(gate_plan(metadata, [selector], root)[:2], ("whole", list(names)), "G3")
        kind, packages, _, reached = gate_plan(
            metadata, ["crates/kernel/src/lib.rs", "scripts/ci/check_sdk_consumer.py"], root)
        self.assertEqual((kind, packages), ("packages", ["kernel"]), "G4")
        self.assertIn("check_sdk_consumer.py", reached, "G4")
        public = gate_plan(metadata, ["scripts/ci/check_public_api.sh"], root, "check_public_api.sh")
        self.assertEqual(public[0], "whole", "G5")

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


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--features", action="store_true")
    parser.add_argument("--production-selection", action="store_true",
                        help="print the production lint's package and feature arguments, one per line")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--source-digest", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--gate-scope", action="store_true")
    parser.add_argument("--entry", default=GATE, help="gate script whose scope is planned (default check-rust.sh)")
    parser.add_argument("--explain", action="store_true", help="also print why each changed path counts (stderr)")
    parser.add_argument("--mode", choices=[mode.removeprefix("--") for mode in HOOK_MODES],
                        help="plan for one check-rust.sh hook mode, quick or push (default both)")
    args = parser.parse_args()
    if args.production_selection:
        if len(sys.argv) != 2:
            parser.error("--production-selection is a standalone mode")
        print("\n".join(production_selection()))
        return 0
    if args.gate_scope:
        if not args.base or args.run or args.features or args.self_test or args.source_digest:
            parser.error("--gate-scope takes only --base, --entry and --explain")
        try:
            base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
            changed = changed_sources(base)
            modes = ("--" + args.mode,) if args.mode else HOOK_MODES
            if args.explain:
                for line in explain(changed, args.entry, modes):
                    print(line, file=sys.stderr)
            if not any(generate_hooks.glob_matches(glob, path) for path in changed
                       for glob in (*rust_inputs(), *sorted(gate_scripts(args.entry, modes)))):
                kind, packages, reason, scripts = "none", [], "no Rust build input or gate script changed", []
            else:
                environment = crate_test.environment(ROOT)
                # Every feature and platform: an edge that exists in any graph a
                # scoped step compiles must reach the consumer set.
                metadata = json.loads(subprocess.check_output(crate_test.native_command(
                    ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"], environment),
                    cwd=ROOT, env=environment))
                kind, packages, reason, scripts = gate_plan(metadata, changed, ROOT, args.entry, modes)
        except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
            print(f"Cannot establish the gate scope: {error}", file=sys.stderr)
            return 1
        print(f"scope={kind}")
        print(f"reason={reason}")
        for script in scripts:
            print(f"script={script}")
        for package in packages:
            print(package)
        return 0
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
        fingerprint, facts = working_subject()
        base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
        changed = changed_sources(base)
        feature_names, companions = crate_test.load_profile(PROFILE)
        environment = crate_test.environment(ROOT)
        metadata = crate_test.full_graph_metadata(feature_names, root=ROOT, environment=environment, host_only=False)
        selected, reason = affected_packages(metadata, changed, ROOT)
        errors = crate_test.companion_errors(metadata, companions, ROOT)
        if errors:
            raise crate_test.PlanError("; ".join(errors))
        commands = crate_test.plan_packages(metadata, feature_names, companions, selected, []) if selected else []
        command = commands[-1] if commands else []
        plan = {"schema_version": 1, "kind": "local-affected", "revision": facts["revision"].decode().strip(), "base": base, "changed": changed, "selected_packages": selected, "reason": reason, "command": command if selected else [], "full_gate": False,
                "source_kind": "working-tree", "source_digest": fingerprint, "index_tree": facts["index_tree"].decode().strip(),
                "untracked_files": {name: hashlib.sha256(data).hexdigest() for name, data in facts["untracked"].items()}}
        plan["commands"] = commands
        # Metadata projects the first capture. This fresh final fence still
        # refuses any late source axis, including no-run/no-work plans.
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed during affected planning; no execution started")
        print(json.dumps(plan, indent=2), flush=True)
        if not args.run or not commands:
            return 0
        directory = Path(environment["CARGO_TARGET_DIR"]) / "test-timings"
        directory.mkdir(parents=True, exist_ok=True)
        receipt = directory / f"affected-{time.time_ns()}.json"
        receipt.with_suffix(".patch").write_bytes(facts["patch"])
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed before affected execution; no execution started")
        started = time.monotonic()
        def verify():
            if source_snapshot()[0] != fingerprint:
                raise ValueError("Source changed before or during affected execution")
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
