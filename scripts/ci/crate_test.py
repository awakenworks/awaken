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

* Cargo compiles the whole authored full-test graph, while a package filter
  retains the requested execution scope. Root node.features alone cannot
  reproduce a peer's feature requests on shared dependency units;
* a `[[companion]]` row of the profile names a binary, the package that builds
  it and the `launcher` crate whose code starts it; a crate whose dependency
  graph reaches the launcher gets the companion built first, with the profile's
  features for that package, into the same target directory;
* `CARGO_BUILD_BUILD_DIR` is set to `CARGO_TARGET_DIR`, because a test resolves
  the companion beside its own executable and a separate build directory moves
  test executables away from the binaries.

`--doc` is the doctest step's half: the same whole graph under
`RUSTC_BOOTSTRAP=1`, so rustdoc checks a `compile_fail,E0xxx` block's pinned
code (ADR-0103). A bare `cargo test -p <crate> --doc` ignores the code and
passes a block that fails for an unrelated reason. Nothing else here sets that
variable: on an ordinary build it would silently admit unstable features.

`--repro-hint <step>` reads a failed gate step's output and prints, per failure
it names, the command above that reproduces it.

usage: crate_test.py [--dry-run] -p <crate> [nextest arguments...]
       crate_test.py [--dry-run] --doc -p <crate> ['<doctest name>']
       crate_test.py --repro-hint test|test-build|doctest < <step output>
       crate_test.py --self-test
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock
from pathlib import Path
import formal_sources

WINDOWS = os.name == "nt"
COMPILE_ENVIRONMENT_OVERRIDES = (
    "CARGO_BUILD_TARGET", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS",
    "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
)

ROOT = Path(__file__).resolve().parents[2]
PROFILE = ROOT / ".config/rust-tests.toml"
USAGE = ("usage: crate_test.py [--dry-run] [--doc] -p <crate> [nextest arguments... | '<doctest name>']"
         " | --repro-hint test|test-build|doctest | --self-test")
# The doctest step's only extra environment (ADR-0103); never an ordinary build's.
DOCTEST_ENVIRONMENT = {"RUSTC_BOOTSTRAP": "1"}
ENTRY = "scripts/ci/test.sh"


class PlanError(Exception):
    """The request cannot be planned; the message says why."""


def load_profile(path: Path = PROFILE) -> tuple[list[str], list[dict[str, str]]]:
    """The full gate's workspace features and the declared companion binaries."""

    document = tomllib.loads(path.read_text(encoding="utf-8"))
    features = document.get("workspace_features")
    if (not isinstance(features, list) or not features
            or not all(isinstance(f, str) and all(f.split("/", 1)) and "/" in f for f in features)
            or len(set(features)) != len(features)):
        raise PlanError(f"{path}: workspace_features must list nonempty unique <package>/<feature> strings")
    companions = document.get("companion", [])
    for row in companions:
        if set(row) != {"package", "binary", "launcher"} or not all(isinstance(v, str) and v for v in row.values()):
            raise PlanError(f"{path}: a [[companion]] row names exactly package, binary and launcher")
    return features, companions


def native_command(command: list[str], environment: dict[str, str] | None = None) -> list[str]:
    """Honor the chosen PATH on Windows, including its .cmd tool wrappers.

    CreateProcess does not search PATHEXT for an unqualified program name.
    Keep logical commands in plans/receipts while resolving the actual launch.
    """
    if not WINDOWS or Path(command[0]).name != command[0]:
        return command
    values = os.environ if environment is None else environment
    program = shutil.which(command[0], path=values.get("PATH", os.environ.get("PATH", os.defpath)))
    if program is None:
        raise FileNotFoundError(2, "Executable not found on the selected PATH", command[0])
    return [program, *command[1:]]


def host_triple(root: Path = ROOT, environment: dict[str, str] | None = None) -> str:
    for line in subprocess.check_output(native_command(["rustc", "-vV"], environment), cwd=root, env=environment, text=True).splitlines():
        if line.startswith("host: "):
            return line.split(" ", 1)[1].strip()
    raise PlanError("rustc -vV reports no host triple")


def full_graph_metadata(features: list[str], *, root: Path = ROOT,
                        environment: dict[str, str] | None = None, host_only: bool = True) -> dict:
    """The one Cargo resolve reader for authored test or compile features.

    Test/compile callers normally use this host. The original local affected
    test planner retains its all-platform conservative selection graph.
    """

    return json.loads(subprocess.check_output(
        native_command(["cargo", "metadata", "--locked", "--format-version", "1",
         *(["--features", ",".join(features)] if features else []),
         *(["--filter-platform", host_triple(root, environment)] if host_only else [])], environment),
        cwd=root, env=environment,
    ))


def _members(metadata: dict) -> dict[str, dict]:
    member_ids = metadata["workspace_members"]
    members = set(member_ids)
    packages = [p for p in metadata["packages"] if p["id"] in members]
    if not members or len(members) != len(member_ids) or {p["id"] for p in packages} != members or len(packages) != len(members):
        raise ValueError("Cargo metadata must contain every unique workspace member")
    if metadata.get("resolve") is None:
        raise ValueError("Cargo's resolved feature/dependency graph is required")
    nodes = metadata["resolve"]["nodes"]
    node_ids = [node["id"] for node in nodes]
    if len(set(node_ids)) != len(node_ids) or not members <= set(node_ids):
        raise ValueError("Cargo's resolved graph omits or duplicates a workspace member")
    package_ids = [p["id"] for p in metadata["packages"]]
    if len(set(package_ids)) != len(package_ids) or not set(node_ids) <= set(package_ids):
        raise ValueError("Cargo's resolved graph contains unknown or duplicate package identities")
    resolved_ids = set(node_ids)
    if any(d["pkg"] not in resolved_ids for node in nodes for d in node["deps"]):
        raise ValueError("Cargo's resolved graph omits a dependency node")
    if len({p["name"] for p in packages}) != len(packages):
        raise ValueError("Cargo's workspace package names must be unique selectors")
    return {package["name"]: package for package in metadata["packages"] if package["id"] in members}


def companion_errors(metadata: dict, companions: list[dict[str, str]], root: Path = ROOT) -> list[str]:
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
        try:
            formal_sources.git_output(root, "grep", "--untracked", "--quiet", "--fixed-strings", binary, "--", f"{sources}/*.rs")
            named = True
        except subprocess.CalledProcessError as error:
            if error.returncode != 1:
                raise
            named = False
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


def plan_packages(metadata: dict, features: list[str], companions: list[dict[str, str]],
                  crates: list[str], passthrough: list[str]) -> list[list[str]]:
    """Compile the original whole graph; filter execution and build companions once.

    Own-root node.features cannot reproduce a peer's transitive requests. Until
    complete test-unit projection is proved, package selection is execution-only.
    This is also the single-root CLI's renderer, not a second implementation.
    """
    members = _members(metadata)
    if not crates or len(set(crates)) != len(crates):
        raise PlanError("selected packages must be nonempty and unique")
    selected = set(crates)
    whole_execution = False
    arguments, filters = [], []
    index = 0
    while index < len(passthrough):
        argument = passthrough[index]
        if argument == "--":
            arguments.extend(passthrough[index:])
            break
        if argument in ("--workspace", "--all"):
            selected = set(members)
            whole_execution = True
        elif argument in ("-p", "--package", "-E", "--filterset"):
            if index + 1 == len(passthrough):
                raise PlanError(f"{argument} requires a value")
            index += 1
            if argument in ("-p", "--package"):
                selected.add(passthrough[index])
            else:
                filters.append(passthrough[index])
        elif argument.startswith("--package="):
            selected.add(argument.split("=", 1)[1])
        elif argument.startswith("--filterset="):
            filters.append(argument.split("=", 1)[1])
        elif argument.startswith("-E") and len(argument) > 2:
            filters.append(argument[2:])
        elif argument.startswith("-p") and len(argument) > 2:
            selected.add(argument[2:])
        elif argument == "--exclude" or argument.startswith("--exclude="):
            raise PlanError("--exclude cannot preserve the required whole test graph")
        elif argument in ("--all-features", "--no-default-features", "--target", "--target-dir", "--manifest-path", "--config", "-Z") or argument.startswith(("--target=", "--target-dir=", "--manifest-path=", "--config=", "-Z")):
            raise PlanError(f"{argument}: graph-changing passthrough is not proved for the whole test profile")
        elif argument in ("--features", "-F") or argument.startswith(("--features=", "-F")):
            if argument in ("--features", "-F"):
                if index + 1 == len(passthrough):
                    raise PlanError(f"{argument} requires a value")
                index += 1
                value = passthrough[index]
                arguments.extend((argument, value))
            else:
                value = argument.split("=", 1)[1] if "=" in argument else argument[2:]
                arguments.append(argument)
            if not set(value.replace(" ", ",").split(",")) <= set(features):
                raise PlanError("extra features differ from the authored whole test profile; no execution started")
        else:
            arguments.append(argument)
        index += 1
    for crate in selected:
        if crate not in members:
            raise PlanError(f"{crate} is not a workspace crate")
    reached = set().union(*(_closure(metadata, members[crate]["id"]) for crate in selected))
    locked = [] if "--locked" in arguments else ["--locked"]
    commands = []
    for row in companions:
        if members[row["launcher"]]["id"] not in reached or selected == {row["package"]}:
            continue
        own = [feature.split("/", 1)[1] for feature in features if feature.split("/", 1)[0] == row["package"]]
        command = ["cargo", "build", "--locked", "-p", row["package"], "--bin", row["binary"],
                   *(["--features", ",".join(own)] if own else []), *_profile_flags(arguments)]
        if command not in commands:
            commands.append(command)
    # Nextest unions repeated user -E arguments. Intersect that union with the
    # original package selection once; simply appending another -E would leak
    # other packages, while ANDing the user's filters would lose valid cases.
    expression = " | ".join(f"({value})" for value in filters)
    if not whole_execution:
        scope = " | ".join(f"package(={name})" for name in sorted(selected))
        expression = f"({scope}) & ({expression})" if expression else scope
    commands.append(["cargo", "nextest", "run", "--workspace", *locked, "--features", ",".join(features),
                     *(["-E", expression] if expression else []), *arguments])
    return commands


def plan_doctests(metadata: dict, features: list[str], crate: str, name: str | None = None,
                  root: Path = ROOT) -> list[str]:
    """The doctest step's command, executing one crate's doctests or one of them.

    The step runs every member's doctests over the whole authored graph. A
    libtest filter keeps that graph and selects execution: rustdoc names a
    doctest `<source path from the workspace root> - <item> (line N)`, so the
    crate's directory selects its doctests and a doctest's source path selects
    the file holding it. Nothing narrower: rustdoc splits a test filter at
    whitespace, so a name (`... - Item (line 57)`) becomes the filter `-` that
    matches every doctest, and `--exact` with it matches none and exits 0.
    """
    package = _members(metadata).get(crate)
    if package is None:
        raise PlanError(f"{crate} is not a workspace crate")
    if not any(target.get("doctest") for target in package["targets"]):
        raise PlanError(f"{crate} has no library target with doctests")
    directory = Path(package["manifest_path"]).parent.relative_to(root).as_posix() + "/"
    source = None if name is None else name.split(" - ", 1)[0]
    if source is not None and not (source.startswith(directory) and source.endswith(".rs")
                                   and not any(c.isspace() for c in source)):
        raise PlanError(f"'{name}' is not a doctest of {crate}: pass the name a failure prints "
                        f"('{directory}src/lib.rs - Item (line N)') or its source path, or none for all")
    return ["cargo", "test", "--workspace", "--locked", "--doc", "--features", ",".join(features),
            "--", source or directory]


ANSI = re.compile(r"\x1b\[[0-9;]*m")
# nextest's per-test and final-summary status lines; a crash is a SIG* status.
NEXTEST_FAILURE = re.compile(
    r"^\s*(?:TRY \d+ )?(?:FAIL|TIMEOUT|LEAK-FAIL|ABORT|SIG[A-Z]+)\s+\[[^\]]*\]\s+"
    r"(?:\(\s*\d+/\s*\d+\)\s+)?(?P<binary>[\w-]+(?:::\S+)?)\s+(?P<test>\S+)\s*$", re.M)
DOCTEST_FAILURE = re.compile(r"^---- (?P<name>\S.* - .*?) stdout ----$", re.M)
COMPILE_FAILURE = re.compile(r"^error: could not compile `(?P<package>[^`]+)`", re.M)
HINT_LIMIT = 10


def package_of(source: str, root: Path = ROOT) -> str | None:
    """The workspace package whose manifest is nearest above a doctest's source path."""

    directory = (root / source).parent
    while directory != root and root in directory.parents:
        manifest = directory / "Cargo.toml"
        if manifest.is_file():
            return tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {}).get("name")
        directory = directory.parent
    return None


def repro_hints(step: str, output: str, root: Path = ROOT) -> list[str]:
    """`→ fix:` lines that reproduce each failure a gate step printed, the way it ran."""

    text = ANSI.sub("", output)
    rows: dict[str, list[str]] = {}
    for package in COMPILE_FAILURE.findall(text):
        rows[f"the build of {package}"] = (
            [ENTRY, "--doc", "-p", package] if step == "doctest" else [ENTRY, "-p", package, "--no-run"])
    if step == "test":
        for match in NEXTEST_FAILURE.finditer(text):
            binary, test = match["binary"], match["test"]
            rows[f"{binary} {test}"] = [ENTRY, "-p", binary.split("::", 1)[0],
                                        "-E", f"binary_id(={binary}) & test(={test})"]
    elif step == "doctest":
        for name in DOCTEST_FAILURE.findall(text):
            package = package_of(name.split(" - ", 1)[0], root)
            rows[name] = [ENTRY, "--doc", "-p", package, name] if package else [ENTRY, "--doc", "-p", "<package>"]
    hints = [f"→ fix: reproduce {subject}: {shlex.join(command)}"
             for subject, command in list(rows.items())[:HINT_LIMIT]]
    if len(rows) > HINT_LIMIT:
        hints.append(f"→ fix: {len(rows) - HINT_LIMIT} more failures reproduce the same way")
    return hints


def environment(root: Path = ROOT) -> dict[str, str]:
    values = dict(os.environ)
    values.setdefault("CARGO_TARGET_DIR", str(root / "target"))
    values["CARGO_BUILD_BUILD_DIR"] = values["CARGO_TARGET_DIR"]
    return values


def execute_commands(commands: list[list[str]], values: dict[str, str], *, root: Path = ROOT,
                     verify=None, record=None, run=None) -> tuple[int, list[dict]]:
    # Every real adapter joins this lifecycle. Tests replace the observer port
    # explicitly; neither the runner nor environment selects an unobserved path.
    from ci_performance import TaskRun
    try:
        # The caller already owns source capture and verify(). Do not perform a
        # second Git scan inside an execution adapter or alter its read order.
        identity = {'available': False, 'scope': 'execution-owner',
                    'source_digest': values.get('AWAKEN_BUILD_SOURCE_DIGEST'),
                    'reason': 'full source receipt remains with the execution owner'}
        task = TaskRun('commands:' + ' '.join(commands[0][:2]) if commands else 'commands:empty',
                       root, values, workload=commands, source_identity=identity)
    except (OSError, ValueError) as error:
        print(f'Cannot initialize build observations: {error}', file=sys.stderr)
        return 74, []
    values = {**values, **{key: task.environment[key] for key in
                          ('AWAKEN_BUILD_RUN_ID', 'AWAKEN_BUILD_RUN_DIRECTORY')}}
    for key in ('AWAKEN_BUILD_WORKLOAD_KIND', 'AWAKEN_BUILD_OBSERVATION_KIND'):
        if key in task.environment:
            values[key] = task.environment[key]
    def retain(rows):
        task.record_commands(rows)
        if record is not None:
            record(rows)
    try:
        status, rows = _execute_commands(commands, values, root=root, verify=verify, record=retain,
                                        run=run)
    except BaseException as error:
        interrupted = 130 if isinstance(error, KeyboardInterrupt) else (
            error.code if isinstance(error, SystemExit) and isinstance(error.code, int) else 1)
        try:
            task.finish(interrupted)
        except (OSError, ValueError) as observation_error:
            print(f'Cannot finish interrupted build observations: {observation_error}', file=sys.stderr)
        raise
    try:
        task.finish(status)
    except (OSError, ValueError) as error:
        print(f'Cannot finish build observations: {error}', file=sys.stderr)
        status = status or 74
    return status, rows


def _execute_commands(commands: list[list[str]], values: dict[str, str], *, root: Path = ROOT,
                      verify=None, record=None, run=None) -> tuple[int, list[dict]]:
    """One ordinary sequence; original failure outranks source/evidence faults."""
    if run is None:
        def run(command, **kwargs):
            return subprocess.run(native_command(command, kwargs.get("env")), **kwargs)
    rows = []
    for command in commands:
        try:
            if verify is not None:
                verify()
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            print(f"Cannot verify command subject: {error}", file=sys.stderr)
            status = error.returncode if isinstance(error, subprocess.CalledProcessError) else 1
            return status if status >= 0 else 128 - status, rows
        print("Command: " + shlex.join(command), flush=True)
        started, error_text = time.monotonic(), None
        try:
            status = run(command, cwd=root, env=values, check=False).returncode
            if status < 0:
                status = 128 - status
        except OSError as error:
            status, error_text = 1, str(error)
        row = {"command": command, "exit_code": status, "duration_seconds": time.monotonic() - started}
        if error_text is not None:
            row["error"] = error_text
        rows.append(row)
        try:
            if verify is not None:
                verify()
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            row["subject_error"] = str(error)
            status = status or 1
        if status != row["exit_code"]:
            row["result_exit_code"] = status
        try:
            if record is not None:
                record(rows)
        except (OSError, ValueError) as error:
            # A consumer asserting an expected compiler refusal must distinguish
            # a retained counterexample from a failed evidence write. This is
            # diagnostic data only; the original exit/first failure still wins.
            row["record_error"] = str(error)
            print(f"Cannot retain command evidence: {error}", file=sys.stderr)
            status = row["exit_code"] or 1
            if "result_exit_code" in row:
                row["result_exit_code"] = status
        if status:
            return status, rows
    return 0, rows


def parse(argv: list[str]) -> tuple[bool, bool, str, list[str]]:
    """(dry run, doctest mode, crate, the rest); the doctest mode takes at most one name."""
    flags = set()
    while argv and argv[0] in ("--dry-run", "--doc"):
        flags.add(argv[0])
        argv = argv[1:]
    if len(argv) < 2 or argv[0] != "-p" or argv[1].startswith("-"):
        raise PlanError(USAGE)
    if "--doc" in flags and (len(argv) > 3 or any(word.startswith("-") for word in argv[2:])):
        raise PlanError("--doc selects every doctest of the crate or one by its full name; "
                        "it takes no other arguments")
    return "--dry-run" in flags, "--doc" in flags, argv[1], argv[2:]


class CrateTestPlan(unittest.TestCase):
    """Cause/effect table, on a synthetic graph and then on this workspace.

    | Rule | the crate                                   | effect                                   |
    |------|---------------------------------------------|------------------------------------------|
    | P1   | reaches launcher in authored whole graph    | companion, whole build + package filter  |
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

    def setUp(self):
        from _build_observation_fixture import synthetic_build_observations
        synthetic_build_observations(self)
        # N6: fixtures cannot act on a hook's foreign repository/index.
        environment = mock.patch.dict(os.environ, formal_sources.detached_git_environment(), clear=True)
        environment.start()
        self.addCleanup(environment.stop)

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
        return plan_packages(self.metadata(), self.features, self.companions, [crate], passthrough or [])

    def test_p1_companion_then_resolved_features(self) -> None:
        # N1/N3: retain the full authored feature requests and whole Cargo graph,
        # not only root-node features; execute exactly the original package.
        self.assertEqual(self.plan_for("cli"), [
            ["cargo", "build", "--locked", "-p", "hand", "--bin", "hand-bin", "--features", "hand"],
            ["cargo", "nextest", "run", "--workspace", "--locked", "--features", "cli/k8s,hand/hand", "-E", "package(=cli)"],
        ])

    def test_p2_p3_no_companion(self) -> None:
        self.assertEqual(self.plan_for("leaf"), [["cargo", "nextest", "run", "--workspace", "--locked", "--features", "cli/k8s,hand/hand", "-E", "package(=leaf)"]])
        self.assertEqual(self.plan_for("hand"),
                         [["cargo", "nextest", "run", "--workspace", "--locked", "--features", "cli/k8s,hand/hand", "-E", "package(=hand)"]])

    def test_p4_p5_p6_passthrough_profile_and_locked(self) -> None:
        commands = self.plan_for("cli", ["--locked", "--cargo-profile", "ci", "-E", "test(x)"])
        self.assertEqual(commands[0][-2:], ["--profile", "ci"])
        self.assertEqual(commands[1], ["cargo", "nextest", "run", "--workspace", "--features", "cli/k8s,hand/hand",
                                       "-E", "(package(=cli)) & ((test(x)))", "--locked", "--cargo-profile", "ci"])
        self.assertEqual(self.plan_for("cli", ["--release"])[0][-1], "--release")

    def test_p7_unknown_crate(self) -> None:
        with self.assertRaisesRegex(PlanError, "nope is not a workspace crate"):
            self.plan_for("nope")

    def test_multiple_roots_and_filtersets_preserve_selected_case_identity(self):
        # N3/N14: root union x Nextest user-filter union, then intersection.
        # The first root has no launcher, the second does: one companion.
        # User OR must not be ANDed, appended as an unbounded -E, or overwritten.
        commands = plan_packages(self.metadata(), self.features, self.companions,
                                 ["leaf"], ["-p", "cli", "-E", "test(x)", "-E", "test(y)", "--no-capture"])
        self.assertEqual(len(commands), 2)
        self.assertEqual(commands[-1][-3:], ["-E", "(package(=cli) | package(=leaf)) & ((test(x)) | (test(y)))", "--no-capture"])
        self.assertNotIn("-p", commands[-1])
        whole = self.plan_for("leaf", ["--workspace", "--features", ",".join(self.features), "-E", "test(x)"])
        self.assertEqual(len(whole), 2)
        self.assertEqual(whole[-1][whole[-1].index("-E") + 1], "(test(x))")
        self.assertEqual(whole[-1][-2:], ["--features", ",".join(self.features)])
        escaped = self.plan_for("leaf", ["--", "literal", "-E", "not-a-cli-filter"])
        self.assertEqual(escaped[-1][-4:], ["--", "literal", "-E", "not-a-cli-filter"])
        for arguments in (["--exclude", "cli"], ["--target", "foreign"], ["--features", "leaf/new"], ["--all-features"], ["-E"]):
            with self.subTest(arguments=arguments), self.assertRaises(PlanError):
                self.plan_for("leaf", arguments)

    def test_peer_features_are_not_reconstructed_from_root_features(self):
        # N1/N2: selected itself has no feature, but a peer requests shared/rich.
        # Root node.features is therefore not a sufficient compile request.
        # The existing fixed point retains peer/shared; tests stay whole until
        # real test-unit projection is proved. No Cargo or feature solver here.
        metadata = self.metadata()
        metadata["resolve"]["nodes"][2]["deps"] = [{"pkg": "shared"}]
        metadata["workspace_members"].extend(("shared", "peer"))
        metadata["packages"].extend({"id": n, "name": n} for n in ("shared", "peer"))
        metadata["resolve"]["nodes"].extend((
            {"id": "shared", "features": ["rich"], "deps": []},
            {"id": "peer", "features": [], "deps": [{"pkg": "shared"}]},
        ))
        command = plan_packages(metadata, self.features, [], ["leaf"], [])[-1]
        self.assertIn("--workspace", command)
        self.assertEqual(command[command.index("--features") + 1], ",".join(self.features))
        self.assertNotIn("shared/rich", command)

    def test_sequence_preserves_status_and_stops_on_source_or_evidence_failure(self):
        # N7-N10: command0/23/75/-15 x post-source/read/writer/spawn faults.
        # 0 becomes failure; original23/75/143 survives every secondary fault;
        # no dependent command starts. Initial source failure starts nothing.
        for raw, expected in ((0, 1), (23, 23), (75, 75), (-15, 143)):
            for cause in ("subject", "record"):
                calls, reads = [], []
                def run(command, **kwargs):
                    calls.append(command)
                    return subprocess.CompletedProcess(command, raw)
                def verify():
                    reads.append(1)
                    if cause == "subject" and len(reads) == 2:
                        raise ValueError("exact source mutation")
                def record(rows):
                    if cause == "record":
                        raise OSError("exact writer fault")
                with self.subTest(raw=raw, cause=cause):
                    status, rows = execute_commands([["first"], ["must-not-run"]], {}, verify=verify, record=record, run=run)
                    self.assertEqual(status, expected)
                    self.assertEqual(calls, [["first"]])
                    self.assertEqual(len(rows), 1)
                    self.assertEqual(rows[0]["command"], calls[0])
        calls = []
        def reject():
            raise subprocess.CalledProcessError(75, ["git", "subject"])
        status, rows = execute_commands([["must-not-run"]], {}, verify=reject, run=lambda *a, **k: calls.append(a))
        self.assertEqual((status, rows, calls), (75, [], []))
        def missing(*args, **kwargs):
            raise OSError("exact spawn fault")
        status, rows = execute_commands([["missing"], ["must-not-run"]], {}, run=missing)
        self.assertEqual(status, 1)
        self.assertEqual(rows[0]["error"], "exact spawn fault")

    def test_companion_git_refusal_is_not_a_stale_row(self):
        # N6/N7: grep miss1 is an ordinary stale row;75/128 is an exact-source
        # read refusal, not a missing binary diagnosis or empty successful plan.
        for status in (75, 128):
            with self.subTest(status=status), mock.patch.object(formal_sources, "git_output", side_effect=subprocess.CalledProcessError(status, ["git", "grep"])):
                with self.assertRaises(subprocess.CalledProcessError) as caught:
                    companion_errors(self.metadata(), self.companions)
                self.assertEqual(caught.exception.returncode, status)
        with mock.patch.object(formal_sources, "git_output", side_effect=subprocess.CalledProcessError(1, ["git", "grep"])):
            self.assertIn("never names it", companion_errors(self.metadata(), self.companions)[0])

    def test_shared_profile_rejects_empty_duplicate_and_unqualified_features(self):
        # N13/N14: the common loader must keep the planner's stronger prior
        # validation, not lower it while removing the duplicate TOML reader.
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / "profile.toml"
            for features in ([], ["cli/k8s", "cli/k8s"], ["bare"], ["/bad"], ["cli/"]):
                profile.write_text("workspace_features = " + json.dumps(features))
                with self.subTest(features=features), self.assertRaises(PlanError):
                    load_profile(profile)

    def test_real_companion_reader_detaches_foreign_hook_and_preserves_decoy(self):
        # N6: actual companion Git consumer x foreign hook DIR/WT/index. It must
        # read its own working source, preserving every decoy source fact; all
        # setup Git uses the existing detached environment, never common config.
        with tempfile.TemporaryDirectory() as directory:
            roots = [Path(directory) / name for name in ("own", "decoy")]
            values = formal_sources.detached_git_environment()
            for root in roots:
                root.mkdir()
                source = root / "crates/launcher/src/lib.rs"
                source.parent.mkdir(parents=True)
                source.write_text('pub const BINARY: &str = "hand-bin";\n' if root == roots[0] else "pub const VALUE: u8 = 1;\n")
                for arguments in (("init", "--quiet"), ("add", "."),
                                  ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                   "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
                    subprocess.run(["git", "-C", str(root), *arguments], env=values, check=True)
            own, decoy = roots
            before = formal_sources.source_facts(decoy)
            protected = {p: p.read_bytes() for p in (decoy / ".git/HEAD", decoy / ".git/index", decoy / ".git/config")}
            metadata = self.metadata()
            for package in metadata["packages"]:
                package["manifest_path"] = str(own / "crates" / package["name"] / "Cargo.toml")
            with mock.patch.dict(os.environ, GIT_DIR=str(decoy / ".git"), GIT_WORK_TREE=str(decoy), GIT_INDEX_FILE=str(decoy / ".git/index")):
                self.assertEqual(companion_errors(metadata, self.companions, own), [])
            self.assertEqual(formal_sources.source_facts(decoy), before)
            self.assertEqual({p: p.read_bytes() for p in protected}, protected)

    def test_native_command_honors_wrappers_without_altering_arguments_or_falling_back(self) -> None:
        # N1 Windows PATH selects .cmd before the installed .exe; N2 omitted
        # tool refuses; N3 explicit executable/Unix argv remains untouched.
        # The real affected-planner CLI table also exercises native .cmd
        # metadata and execution, preserving original status and receipts.
        command = ["cargo", "nextest", "-E", "package(=cli) & test(value)"]
        values = {"PATH": "chosen-tools"}
        with mock.patch(__name__ + ".WINDOWS", True), mock.patch.object(shutil, "which", return_value="chosen-tools/cargo.cmd") as find:
            self.assertEqual(native_command(command, values), ["chosen-tools/cargo.cmd", *command[1:]])
            find.assert_called_once_with("cargo", path="chosen-tools")
            self.assertEqual(command[0], "cargo")
        with mock.patch(__name__ + ".WINDOWS", True), mock.patch.object(shutil, "which", return_value=None):
            with self.assertRaises(FileNotFoundError):
                native_command(command, values)
        for windows, explicit in [(False, command), (True, ["chosen-tools/cargo.cmd", *command[1:]])]:
            with mock.patch(__name__ + ".WINDOWS", windows), mock.patch.object(shutil, "which") as find:
                self.assertEqual(native_command(explicit, values), explicit)
                find.assert_not_called()
        with mock.patch(__name__ + ".native_command", return_value=["chosen-tools/cargo.cmd", *command[1:]]), \
                mock.patch("ci_performance.TaskRun") as observation, \
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(command, 0)) as spawn:
            # This table owns native argument forwarding; the observation
            # writer's real Git/storage lifecycle has its own I1-I13 table.
            observation.return_value.environment = {
                'AWAKEN_BUILD_RUN_ID': 'fixture', 'AWAKEN_BUILD_RUN_DIRECTORY': '/fixture'}
            status, rows = execute_commands([command], values)
            self.assertEqual(status, 0)
            self.assertEqual(rows[0]["command"], command)
            self.assertEqual(spawn.call_args.args[0], ["chosen-tools/cargo.cmd", *command[1:]])
        calls = []
        def record_run(actual, **kwargs):
            calls.append(actual)
            return subprocess.CompletedProcess(actual, 0)
        with mock.patch(__name__ + ".native_command", side_effect=AssertionError("adapter owns the launch")):
            status, rows = execute_commands([command], values, run=record_run)
            self.assertEqual((status, calls, rows[0]["command"]), (0, [command], command))

    def test_metadata_owner_preserves_root_environment_features_and_host_mode(self) -> None:
        # B10: both callers use this original metadata owner. The compile caller
        # passes its authored features/root/BUILD environment; the old affected
        # nextest mode requests the all-platform graph. Neither invents a profile
        # or compiler feature solver. Empty features must not mean --features ''.
        root, values = Path("/fixture/root"), {"CARGO_TARGET_DIR": "/fixture/target"}
        with mock.patch(__name__ + ".host_triple", return_value="fixture-host") as host, \
                mock.patch.object(subprocess, "check_output", return_value="{}") as output:
            self.assertEqual(full_graph_metadata(["cli/k8s"], root=root, environment=values), {})
            host.assert_called_once_with(root, values)
            output.assert_called_once_with(native_command(["cargo", "metadata", "--locked", "--format-version", "1", "--features", "cli/k8s", "--filter-platform", "fixture-host"], values), cwd=root, env=values)
        with mock.patch(__name__ + ".host_triple", side_effect=AssertionError("host must not be read")), \
                mock.patch.object(subprocess, "check_output", return_value="{}") as output:
            full_graph_metadata([], root=root, environment=values, host_only=False)
            output.assert_called_once_with(native_command(["cargo", "metadata", "--locked", "--format-version", "1"], values), cwd=root, env=values)

    def test_metadata_refusal_starts_no_companion_or_nextest(self) -> None:
        # B12: original metadata75/23/signal refusal -> same public CLI status,
        # zero companion/test processes. This is the existing owner entry point,
        # not a duplicate runner or a successful empty plan.
        for original, expected in ((75, 75), (23, 23), (-15, 143)):
            with self.subTest(status=original), \
                    mock.patch("rust_test_plan.source_snapshot", return_value=("known-working-subject", b"")), \
                    mock.patch(__name__ + ".full_graph_metadata", side_effect=subprocess.CalledProcessError(original, ["cargo", "metadata"])), \
                    mock.patch.object(subprocess, "run", side_effect=AssertionError("dependent execution started")):
                self.assertEqual(main(["-p", "awaken-cli"]), expected)

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
            # Reuse the existing native shell authority, never a WSL substrate.
            from _native_shell import bash_executable
            command.insert(0, bash_executable())
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
            self.assertIn(f"package(={crate})", test[test.index("-E") + 1])
            self.assertIn("awaken-cli/container-k8s", test[test.index("--features") + 1].split(","))
            self.assertEqual(test[-1], "--no-fail-fast", "nextest arguments come last")
        leaf = self.dry_run("awaken-runtime")
        self.assertEqual(len(leaf), 1, "D3 a crate that launches nothing builds no companion")
        self.assertIn("awaken-cli/container-k8s", leaf[0][leaf[0].index("--features") + 1])


class ReproEntry(unittest.TestCase):
    """Repro entry decision table; synthetic metadata and output, no Cargo.

    | Rule | input                                        | effect                                         |
    |------|----------------------------------------------|------------------------------------------------|
    | R1   | --doc -p <crate> [one full name]             | parsed; extra or flag arguments refused        |
    | R2   | doctest plan                                 | whole graph, crate directory or --exact name   |
    | R3   | --doc vs test mode execution environment     | RUSTC_BOOTSTRAP=1 only on the doctest command  |
    | R4   | nextest / doctest / compile failure output   | one `→ fix:` command per failure, capped       |
    | R5   | a printed test hint, parsed back             | the plan builds the companion first            |
    """

    features = CrateTestPlan.features

    def setUp(self):
        environment = mock.patch.dict(os.environ, {"PATH": os.environ.get("PATH", "")}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def metadata(self) -> dict:
        metadata = CrateTestPlan.metadata()
        for package in metadata["packages"]:
            for target in package["targets"]:
                target["doctest"] = target["kind"] == ["lib"]
        return metadata

    def test_r1_doc_arguments(self):
        self.assertEqual(parse(["--doc", "-p", "leaf"]), (False, True, "leaf", []))
        self.assertEqual(parse(["--dry-run", "--doc", "-p", "leaf", "x - y (line 1)"]),
                         (True, True, "leaf", ["x - y (line 1)"]))
        self.assertEqual(parse(["-p", "leaf", "--no-capture"]), (False, False, "leaf", ["--no-capture"]))
        for argv in (["--doc", "-p", "leaf", "a", "b"], ["--doc", "-p", "leaf", "--no-capture"], ["--doc"]):
            with self.subTest(argv=argv), self.assertRaises(PlanError):
                parse(argv)

    def test_r2_doctest_plan(self):
        whole = ["cargo", "test", "--workspace", "--locked", "--doc", "--features", "cli/k8s,hand/hand", "--"]
        self.assertEqual(plan_doctests(self.metadata(), self.features, "leaf"), [*whole, "crates/leaf/"])
        # rustdoc splits filters at whitespace, so a named doctest selects its
        # source file: listed name (` - compile fail`), failure header, or path.
        for name in ("crates/leaf/src/lib.rs - Lock (line 57) - compile fail",
                     "crates/leaf/src/lib.rs - Lock (line 57)", "crates/leaf/src/lib.rs"):
            command = plan_doctests(self.metadata(), self.features, "leaf", name)
            self.assertEqual(command, [*whole, "crates/leaf/src/lib.rs"])
            self.assertFalse(any(c.isspace() for c in command[-1]))
        for crate, name, refusal in (("leaf", "crates/cli/src/lib.rs - X (line 1)", "not a doctest of leaf"),
                                     ("leaf", "Lock", "not a doctest of leaf"),
                                     ("leaf", "crates/leaf/src/my lib.rs - Lock (line 1)", "not a doctest of leaf"),
                                     ("hand", None, "no library target"), ("nope", None, "not a workspace")):
            with self.subTest(crate=crate, name=name), self.assertRaisesRegex(PlanError, refusal):
                plan_doctests(self.metadata(), self.features, crate, name)

    def run_main(self, argv: list[str]) -> tuple[int, dict, str]:
        import contextlib
        import io
        printed = io.StringIO()
        with mock.patch("rust_test_plan.source_snapshot", return_value=("subject", b"")), \
                mock.patch(__name__ + ".load_profile", return_value=(self.features, CrateTestPlan.companions)), \
                mock.patch(__name__ + ".full_graph_metadata", return_value=self.metadata()), \
                mock.patch(__name__ + ".companion_errors", return_value=[]), \
                mock.patch(__name__ + ".execute_commands", return_value=(0, [])) as execute, \
                contextlib.redirect_stdout(printed):
            status = main(argv)
        return status, execute.call_args, printed.getvalue()

    def test_r3_only_the_doctest_command_sets_rustc_bootstrap(self):
        status, call, printed = self.run_main(["--doc", "-p", "leaf"])
        self.assertEqual(status, 0)
        self.assertEqual(call.args[1]["RUSTC_BOOTSTRAP"], "1")
        self.assertEqual(call.args[0][0][:5], ["cargo", "test", "--workspace", "--locked", "--doc"])
        self.assertIn("RUSTC_BOOTSTRAP=1 cargo test", printed)
        status, call, printed = self.run_main(["-p", "cli"])
        self.assertEqual(status, 0)
        self.assertNotIn("RUSTC_BOOTSTRAP", call.args[1])
        self.assertNotIn("RUSTC_BOOTSTRAP", printed)
        self.assertEqual(self.run_main(["--dry-run", "--doc", "-p", "leaf"])[1], None, "dry run executes nothing")

    def test_r4_hints_name_each_failure_once(self):
        nextest = ("        FAIL [   0.004s] ( 1/20) cli::scenario tests::boom\n"
                   "──── STDERR:             cli::scenario tests::boom\n"
                   "\x1b[31m     SIGABRT\x1b[0m [   0.100s] (2/20) cli tests::deep\n"
                   "     Summary [   0.105s] 20 tests run: 18 passed, 2 failed, 0 skipped\n"
                   "        FAIL [   0.004s] ( 1/20) cli::scenario tests::boom\n"
                   "        PASS [   0.004s] ( 3/20) cli tests::fine\n")
        self.assertEqual(repro_hints("test", nextest), [
            "→ fix: reproduce cli::scenario tests::boom: scripts/ci/test.sh -p cli "
            "-E 'binary_id(=cli::scenario) & test(=tests::boom)'",
            "→ fix: reproduce cli tests::deep: scripts/ci/test.sh -p cli -E 'binary_id(=cli) & test(=tests::deep)'"])
        build = "error[E0425]: x\nerror: could not compile `leaf` (test \"t\") due to 1 previous error\n"
        self.assertEqual(repro_hints("test-build", build),
                         ["→ fix: reproduce the build of leaf: scripts/ci/test.sh -p leaf --no-run"])
        self.assertEqual(repro_hints("doctest", build),
                         ["→ fix: reproduce the build of leaf: scripts/ci/test.sh --doc -p leaf"])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "crates/leaf/src").mkdir(parents=True)
            (root / "crates/leaf/Cargo.toml").write_text('[package]\nname = "leaf"\n')
            name = "crates/leaf/src/lib.rs - Lock (line 57) - compile fail"
            doctest = f"test {name} ... FAILED\n\nfailures:\n\n---- {name} stdout ----\nerror[E0599]\n"
            self.assertEqual(repro_hints("doctest", doctest, root),
                             [f"→ fix: reproduce {name}: scripts/ci/test.sh --doc -p leaf '{name}'"])
        many = "".join(f"        FAIL [ 0.1s] cli tests::case{n}\n" for n in range(HINT_LIMIT + 3))
        hints = repro_hints("test", many)
        self.assertEqual(len(hints), HINT_LIMIT + 1)
        self.assertEqual(hints[-1], "→ fix: 3 more failures reproduce the same way")
        self.assertEqual(repro_hints("test", "error: test run failed\n"), [])

    def test_r5_a_test_hint_plans_the_companion_first(self):
        hint = repro_hints("test", "        FAIL [ 0.1s] cli::scenario tests::boom\n")[0]
        _, doc, crate, rest = parse(shlex.split(hint.split(": ", 2)[2])[1:])
        commands = plan_packages(self.metadata(), self.features, CrateTestPlan.companions, [crate], rest)
        self.assertFalse(doc)
        self.assertEqual(commands[0][:2] + commands[0][4:7], ["cargo", "build", "hand", "--bin", "hand-bin"])
        self.assertEqual(commands[1][-2:], ["-E", "(package(=cli)) & ((binary_id(=cli::scenario) & test(=tests::boom)))"])


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        loader = unittest.defaultTestLoader
        suite = unittest.TestSuite([loader.loadTestsFromTestCase(CrateTestPlan),
                                    loader.loadTestsFromTestCase(ReproEntry)])
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if argv[:1] == ["--repro-hint"]:
        if len(argv) != 2 or argv[1] not in ("test", "test-build", "doctest"):
            print(USAGE, file=sys.stderr)
            return 2
        print("\n".join(repro_hints(argv[1], sys.stdin.read())))
        return 0
    try:
        dry_run, doc, crate, passthrough = parse(argv)
        # N5/N9: the shared working view binds the index and WT before metadata;
        # the imports are at this consumer entry, never in the graph helpers.
        from rust_test_plan import source_snapshot
        fingerprint, _ = source_snapshot()
        features, companions = load_profile()
        values = environment()
        metadata = full_graph_metadata(features, root=ROOT, environment=values, host_only=False)
        if doc:
            commands = [plan_doctests(metadata, features, crate, *passthrough)]
            values.update(DOCTEST_ENVIRONMENT)
        else:
            errors = companion_errors(metadata, companions, ROOT)
            if errors:
                raise PlanError("; ".join(errors) + f" -- fix the [[companion]] rows in {PROFILE.relative_to(ROOT)}")
            commands = plan_packages(metadata, features, companions, [crate], passthrough)
        if source_snapshot()[0] != fingerprint:
            raise PlanError("Source changed during test planning; no execution started")
    except PlanError as error:
        print(error, file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as error:
        # A refused metadata/read cannot become an empty successful plan or
        # start a companion. Keep admission75 and every original positive code.
        return error.returncode if error.returncode >= 0 else 128 - error.returncode
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot establish test working subject or plan: {error}", file=sys.stderr)
        return 1
    prefix = [f"CARGO_TARGET_DIR={values['CARGO_TARGET_DIR']}", f"CARGO_BUILD_BUILD_DIR={values['CARGO_BUILD_BUILD_DIR']}",
              *(f"{key}={value}" for key, value in DOCTEST_ENVIRONMENT.items() if doc)]
    for command in commands:
        print(shlex.join(prefix + command), flush=True)
    if dry_run:
        return 0
    def verify():
        if source_snapshot()[0] != fingerprint:
            raise ValueError("Source changed before or during test execution")
    values['AWAKEN_BUILD_SOURCE_DIGEST'] = fingerprint
    return execute_commands(commands, values, root=ROOT, verify=verify)[0]


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
