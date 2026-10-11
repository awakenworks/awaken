#!/usr/bin/env python3
"""Run one crate's tests in the graph the full gate runs them in.

`cargo nextest run -p <crate>` alone is not that graph. The full gate tests the
whole workspace with the features in `.config/rust-tests.toml`, and Cargo
unifies them onto every member: `awaken-cli/container-k8s` turns on
`container-k8s` in awaken-coordinator too. Some tests also launch a companion
binary from beside their own executable -- the Hand companion `awaken-sandbox`
-- which no crate under test depends on, so `-p` never builds it. Both gaps
used to surface as red tests that were not failures: a missing feature, or a
bare "No such file" for the companion.

This derives everything from those two sources instead of a second list:

* the crate's features are the ones Cargo resolves onto it in the full-gate
  graph (`cargo metadata --features <workspace_features>`);
* a `[[companion]]` row of the profile names a binary, the package that builds
  it and the `launcher` crate whose code starts it; a crate whose dependency
  graph reaches the launcher gets the companion built first, with the profile's
  features for that package, into the same target directory;
* `CARGO_BUILD_BUILD_DIR` is set to `CARGO_TARGET_DIR`, because a test resolves
  the companion beside its own executable and a separate build directory moves
  test executables away from the binaries.

usage: crate_test.py [--dry-run] -p <crate> [nextest arguments...] | --self-test
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROFILE = ROOT / ".config/rust-tests.toml"
USAGE = "usage: crate_test.py [--dry-run] -p <crate> [nextest arguments...] | --self-test"


class PlanError(Exception):
    """The request cannot be planned; the message says why."""


def load_profile(path: Path = PROFILE) -> tuple[list[str], list[dict[str, str]]]:
    """The full gate's workspace features and the declared companion binaries."""

    document = tomllib.loads(path.read_text(encoding="utf-8"))
    features = document.get("workspace_features")
    if not isinstance(features, list) or not all(isinstance(f, str) and "/" in f for f in features):
        raise PlanError(f"{path}: workspace_features must list <package>/<feature> strings")
    companions = document.get("companion", [])
    for row in companions:
        if set(row) != {"package", "binary", "launcher"} or not all(isinstance(v, str) and v for v in row.values()):
            raise PlanError(f"{path}: a [[companion]] row names exactly package, binary and launcher")
    return features, companions


def host_triple() -> str:
    for line in subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True).splitlines():
        if line.startswith("host: "):
            return line.split(" ", 1)[1].strip()
    raise PlanError("rustc -vV reports no host triple")


def full_graph_metadata(features: list[str]) -> dict:
    """Cargo's resolve for the full gate's feature set, on this host."""

    return json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--features", ",".join(features),
         "--filter-platform", host_triple()],
        cwd=ROOT,
    ))


def _members(metadata: dict) -> dict[str, dict]:
    members = set(metadata["workspace_members"])
    return {package["name"]: package for package in metadata["packages"] if package["id"] in members}


def companion_errors(metadata: dict, companions: list[dict[str, str]]) -> list[str]:
    """A companion row that no longer describes the workspace refuses to plan."""

    members = _members(metadata)
    errors = []
    for row in companions:
        package, binary, launcher = row["package"], row["binary"], row["launcher"]
        builder = members.get(package)
        if builder is None or not any(
            target["name"] == binary and "bin" in target["kind"] for target in builder["targets"]
        ):
            errors.append(f"companion {binary}: workspace package {package} builds no such binary")
        owner = members.get(launcher)
        if owner is None:
            errors.append(f"companion {binary}: launcher {launcher} is not a workspace crate")
            continue
        # The scope is the launcher's own source tree, located by Cargo metadata.
        sources = Path(owner["manifest_path"]).parent / "src"
        named = subprocess.run(
            ["git", "grep", "--untracked", "--quiet", "--fixed-strings", binary, "--", f"{sources}/*.rs"],
            cwd=ROOT, check=False,
        ).returncode == 0
        if not named:
            errors.append(f"companion {binary}: launcher {launcher} never names it, so it does not start it")
    return errors


def _closure(metadata: dict, package_id: str) -> set[str]:
    edges = {node["id"]: [dependency["pkg"] for dependency in node["deps"]] for node in metadata["resolve"]["nodes"]}
    reached, pending = {package_id}, [package_id]
    while pending:
        for dependency in edges.get(pending.pop(), ()):
            if dependency not in reached:
                reached.add(dependency)
                pending.append(dependency)
    return reached


def _profile_flags(passthrough: list[str]) -> list[str]:
    """The Cargo profile the nextest arguments select, as `cargo build` spells it."""

    for index, argument in enumerate(passthrough):
        if argument in ("--release", "-r"):
            return ["--release"]
        if argument == "--cargo-profile" and index + 1 < len(passthrough):
            return ["--profile", passthrough[index + 1]]
        if argument.startswith("--cargo-profile="):
            return ["--profile", argument.split("=", 1)[1]]
    return []


def plan(metadata: dict, features: list[str], companions: list[dict[str, str]], crate: str,
         passthrough: list[str]) -> list[list[str]]:
    """The commands that test `crate`: companion builds first, the test run last."""

    members = _members(metadata)
    if crate not in members:
        raise PlanError(f"{crate} is not a workspace crate")
    identity = members[crate]["id"]
    node = next(node for node in metadata["resolve"]["nodes"] if node["id"] == identity)
    reached = {package["name"] for package in metadata["packages"] if package["id"] in _closure(metadata, identity)}
    locked = [] if "--locked" in passthrough else ["--locked"]
    commands = []
    for row in companions:
        if row["launcher"] not in reached or row["package"] == crate:
            continue
        own = [feature.split("/", 1)[1] for feature in features if feature.split("/", 1)[0] == row["package"]]
        commands.append(["cargo", "build", "--locked", "-p", row["package"], "--bin", row["binary"],
                         *(["--features", ",".join(own)] if own else []), *_profile_flags(passthrough)])
    resolved = sorted(feature for feature in node["features"] if feature != "default")
    commands.append(["cargo", "nextest", "run", *locked, "-p", crate,
                     *(["--features", ",".join(resolved)] if resolved else []), *passthrough])
    return commands


def environment() -> dict[str, str]:
    values = dict(os.environ)
    values.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    values["CARGO_BUILD_BUILD_DIR"] = values["CARGO_TARGET_DIR"]
    return values


def plan_packages(metadata: dict, features: list[str], companions: list[dict[str, str]],
                  crates: list[str], passthrough: list[str]) -> list[list[str]]:
    """Build each companion once, then build/test only the selected packages."""
    if not crates or len(set(crates)) != len(crates):
        raise PlanError("selected packages must be nonempty and unique")
    commands: list[list[str]] = []
    for crate in crates:
        for command in plan(metadata, features, companions, crate, passthrough)[:-1]:
            if command not in commands:
                commands.append(command)
    members = _members(metadata)
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    resolved = sorted(f"{crate}/{feature}" for crate in crates
                      for feature in nodes[members[crate]["id"]]["features"] if feature != "default")
    locked = [] if "--locked" in passthrough else ["--locked"]
    commands.append(["cargo", "nextest", "run", *locked,
                     *(argument for crate in crates for argument in ("-p", crate)),
                     *(["--features", ",".join(resolved)] if resolved else []), *passthrough])
    return commands


def parse(argv: list[str]) -> tuple[bool, str, list[str]]:
    dry_run = False
    while argv and argv[0] == "--dry-run":
        dry_run, argv = True, argv[1:]
    if len(argv) < 2 or argv[0] != "-p" or argv[1].startswith("-"):
        raise PlanError(USAGE)
    return dry_run, argv[1], argv[2:]


class CrateTestPlan(unittest.TestCase):
    """Cause/effect table, on a synthetic graph and then on this workspace.

    | Rule | the crate                                   | effect                                   |
    |------|---------------------------------------------|------------------------------------------|
    | P1   | reaches the launcher, has resolved features | companion build, then nextest + features |
    | P2   | reaches no launcher, resolves no feature    | nextest alone, no --features             |
    | P3   | is the companion's own package              | no separate companion build              |
    | P4   | gets nextest arguments                      | passed verbatim, last                    |
    | P5   | selects a Cargo profile (--release, ...)    | the companion is built in that profile   |
    | P6   | gets --locked from the caller               | not repeated                             |
    | P7   | is not a workspace crate                    | refused, naming it                       |
    | P8   | companion row no longer matches the tree    | refused, naming the row                  |
    | D1-3 | awaken-cli / awaken-coordinator / a leaf    | real --dry-run rows through test.sh      |
    """

    features = ["cli/k8s", "hand/hand"]
    companions = [{"package": "hand", "binary": "hand-bin", "launcher": "launcher"}]

    @staticmethod
    def metadata() -> dict:
        def package(name: str, *targets: tuple[str, str]) -> dict:
            return {"id": name, "name": name, "manifest_path": str(ROOT / "crates" / name / "Cargo.toml"),
                    "targets": [{"name": target, "kind": [kind]} for target, kind in targets]}

        graph = {"cli": (["k8s", "default"], ["launcher"]), "launcher": ([], []), "leaf": ([], []),
                 "hand": (["hand"], [])}
        return {
            "workspace_members": list(graph),
            "packages": [package("cli", ("cli", "lib")), package("launcher", ("launcher", "lib")),
                         package("leaf", ("leaf", "lib")), package("hand", ("hand-bin", "bin"))],
            "resolve": {"nodes": [{"id": name, "features": features, "deps": [{"pkg": d} for d in deps]}
                                  for name, (features, deps) in graph.items()]},
        }

    def plan_for(self, crate: str, passthrough: list[str] | None = None) -> list[list[str]]:
        return plan(self.metadata(), self.features, self.companions, crate, passthrough or [])

    def test_p1_companion_then_resolved_features(self) -> None:
        self.assertEqual(self.plan_for("cli"), [
            ["cargo", "build", "--locked", "-p", "hand", "--bin", "hand-bin", "--features", "hand"],
            ["cargo", "nextest", "run", "--locked", "-p", "cli", "--features", "k8s"],
        ])

    def test_p2_p3_no_companion(self) -> None:
        self.assertEqual(self.plan_for("leaf"), [["cargo", "nextest", "run", "--locked", "-p", "leaf"]])
        self.assertEqual(self.plan_for("hand"),
                         [["cargo", "nextest", "run", "--locked", "-p", "hand", "--features", "hand"]])

    def test_p4_p5_p6_passthrough_profile_and_locked(self) -> None:
        commands = self.plan_for("cli", ["--locked", "--cargo-profile", "ci", "-E", "test(x)"])
        self.assertEqual(commands[0][-2:], ["--profile", "ci"])
        self.assertEqual(commands[1], ["cargo", "nextest", "run", "-p", "cli", "--features", "k8s",
                                       "--locked", "--cargo-profile", "ci", "-E", "test(x)"])
        self.assertEqual(self.plan_for("cli", ["--release"])[0][-1], "--release")

    def test_p7_unknown_crate(self) -> None:
        with self.assertRaisesRegex(PlanError, "nope is not a workspace crate"):
            self.plan_for("nope")

    def test_p8_stale_companion_rows(self) -> None:
        rows = [{"package": "leaf", "binary": "hand-bin", "launcher": "gone"}]
        errors = companion_errors(self.metadata(), rows)
        self.assertIn("companion hand-bin: workspace package leaf builds no such binary", errors)
        self.assertIn("companion hand-bin: launcher gone is not a workspace crate", errors)
        metadata, _, companions = self.workspace()
        self.assertEqual(companion_errors(metadata, companions), [])
        silent = [{**companions[0], "launcher": "awaken-runtime"}]
        self.assertEqual(companion_errors(metadata, silent),
                         [f"companion {companions[0]['binary']}: launcher awaken-runtime never names it, "
                          "so it does not start it"])

    _workspace: tuple | None = None

    @classmethod
    def workspace(cls) -> tuple:
        if cls._workspace is None:
            features, companions = load_profile()
            cls._workspace = (full_graph_metadata(features), features, companions)
        return cls._workspace

    def dry_run(self, crate: str) -> list[list[str]]:
        command = [str(ROOT / "scripts/ci/test.sh"), "--dry-run", "-p", crate, "--no-fail-fast"]
        if os.name == "nt":
            # CreateProcess cannot execute a POSIX shebang. Select native Git
            # Bash explicitly; a System32 WSL bash is a different substrate.
            bash = os.environ.get("AWAKEN_NATIVE_BASH") or str(
                Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git/bin/bash.exe")
            if not Path(bash).is_file():
                raise PlanError("native shell self-test requires Git Bash or AWAKEN_NATIVE_BASH")
            command = [bash, *command]
        result = subprocess.run(command,
                                cwd=ROOT, capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        return [shlex.split(line) for line in result.stdout.splitlines() if line.strip()]

    def test_d1_d2_d3_dry_run_rows(self) -> None:
        companion = self.workspace()[2][0]
        for crate in ("awaken-cli", "awaken-coordinator"):
            *builds, test = self.dry_run(crate)
            self.assertEqual(len(builds), 1, f"D1/D2 {crate} builds its companion first")
            target = next(word for word in builds[0] if word.startswith("CARGO_TARGET_DIR="))
            self.assertIn("CARGO_BUILD_BUILD_DIR=" + target.split("=", 1)[1], builds[0])
            self.assertIn(companion["binary"], builds[0])
            self.assertIn("hand", builds[0][builds[0].index("--features") + 1].split(","))
            self.assertEqual(test[test.index("-p") + 1], crate)
            self.assertIn("container-k8s", test[test.index("--features") + 1].split(","))
            self.assertEqual(test[-1], "--no-fail-fast", "nextest arguments come last")
        leaf = self.dry_run("awaken-runtime")
        self.assertEqual(len(leaf), 1, "D3 a crate that launches nothing builds no companion")
        self.assertNotIn("--features", leaf[0])


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(CrateTestPlan)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        dry_run, crate, passthrough = parse(argv)
        features, companions = load_profile()
        metadata = full_graph_metadata(features)
        errors = companion_errors(metadata, companions)
        if errors:
            raise PlanError("; ".join(errors) + f" -- fix the [[companion]] rows in {PROFILE.relative_to(ROOT)}")
        commands = plan(metadata, features, companions, crate, passthrough)
    except PlanError as error:
        print(error, file=sys.stderr)
        return 2
    values = environment()
    prefix = [f"CARGO_TARGET_DIR={values['CARGO_TARGET_DIR']}", f"CARGO_BUILD_BUILD_DIR={values['CARGO_BUILD_BUILD_DIR']}"]
    for command in commands:
        print(shlex.join(prefix + command), flush=True)
        if dry_run:
            continue
        status = subprocess.run(command, cwd=ROOT, env=values, check=False).returncode
        if status:
            return status
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
