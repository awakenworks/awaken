#!/usr/bin/env python3
"""Select the suites of a service-backed group (postgres, reliability) a change can reach.

`pg_tests.sh --affected <base>` and `check-reliability.sh --affected <base>`
ask this planner which of their suites to run; the shells only look the
decision up by the line that starts the suite (`_affected_suites.sh`). It reuses
the Rust gate's planner (`rust_test_plan.py`): the same changed-path union, the
same Rust input globs and the same Cargo ownership walk over owners and their
transitive consumers, including test dependencies.

Scope decision table (one change set, one group):
  S1 no path the group reads changed (docs, web, a CI script the group never
     runs or names)                                              -> none
  S2 a group script: the entry, anything it sources, runs or imports (this
     planner and rust_test_plan.py included), its caller check-all.sh, or a
     data file under scripts/ one of them names                  -> whole
  S3 a manifest, the lockfile, .config profiles, the toolchain or a Rust
     input no workspace crate owns                               -> whole
  S4 member-crate sources only                                   -> packages
Suite mapping (a suite is one command the group's wrapper runs):
  M1 its packages are the -p/--package/--test-package values it names;
  M2 `cargo fuzz run` maps to the fuzz crate's path dependencies;
  M3 a command that runs another scripts/ci shell script maps to the packages
     that script's suites name; SUBSTRATE_ONLY scripts run only with S2/S3;
  M4 a FOLLOWS command shares the decision of the suite just before it;
  M5 anything else, an unknown package, an unknown stage or a test command
     outside the wrapper is a hard error, never a silent skip.
Under S4 a suite runs when one of its packages is an owner or a consumer.
"""
from __future__ import annotations

import argparse
import ast
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import tomllib

import crate_test
import generate_hooks
import rust_test_plan
from _crate_boundary_workspace import crate_manifests

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "scripts/ci"
# Inputs of the suite map the self-test checks against the real groups.
INPUT_GLOBS = ("crates/**/Cargo.toml", "fuzz/Cargo.toml", "scripts/ci/*.py", "scripts/ci/*.sh",
               "scripts/ci/pre-commit/*.sh", "scripts/ci/pre-push/*.sh")
OWNED = "changed owners and transitive consumers, including test dependencies"
CALLERS = ("check-all.sh",)
RULE = "rule: every suite of an affected group maps to the packages it tests"
WHY = "why: an affected run skips a suite it cannot map, so a missing mapping would hide a red suite"


@dataclass(frozen=True)
class Group:
    entry: str
    wrappers: tuple[str, ...]
    excluded: tuple[str, str]  # (first line, closing line) of a block that holds no group suite


GROUPS = {
    # self_test() stubs the wrappers; its lines are fixtures, not suites.
    "postgres": Group("pg_tests.sh", ("pg_required_test", "pg_test", "pg_run_with_deadline"),
                      (r"self_test\(\) \{", r"\}")),
    # --session-observations is a separate evidence mode that --affected never narrows.
    "reliability": Group("check-reliability.sh", ("affected_suite",),
                         (r'if \[ "\$\{1:-\}" = "--session-observations" \]; then', r"fi")),
}
# pg_run_with_deadline stages that set up, observe or clean the substrate.
INFRA_STAGES = ("docker-info", "postgres-create", "postgres-port", "postgres-ready",
                "object-create", "object-port", "object-ready", "object-policy")
INFRA_STAGE_PREFIXES = ("cleanup-", "diagnostic-", "test:")
FOLLOWS = {
    "object-credential-rotation": "rotates the object-store fixture credential between its seed and verify suites",
    "scripts/ci/check_reliability.py": "judges the evidence the suite before it wrote",
}
SUBSTRATE_ONLY = {
    "scripts/ci/postgres_recovery_drill.sh": "point-in-time restore of the Postgres and MinIO substrates; it runs no workspace crate",
}
CARGO_INFRA = ("clean", "install", "metadata")
KEYWORDS = frozenset({"if", "then", "elif", "else", "do", "while", "until", "!", "{", "}", "time", "fi", "done", "esac"})
ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
SHELL_SCRIPT = re.compile(r"scripts/ci/[A-Za-z0-9_.-]+\.sh")


class PlanError(Exception):
    pass


@dataclass(frozen=True)
class Suite:
    file: str
    line: int
    text: str
    packages: tuple[str, ...]
    rule: str  # "packages", "follows" or "substrate"
    note: str = ""

    @property
    def key(self) -> str:
        return f"{self.file}:{self.line}"


def _tokens(text: str) -> list[str]:
    lexer = shlex.shlex(text, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    return list(lexer)


def commands(path: Path, excluded: tuple[str, str] | None = None) -> list[tuple[int, list[str]]]:
    """``(first line, words)`` of every simple command in a shell script."""
    lines = path.read_text(encoding="utf-8").splitlines()
    skip: set[int] = set()
    if excluded:
        start = next((i for i, line in enumerate(lines) if re.fullmatch(excluded[0], line)), None)
        end = None if start is None else next(
            (i for i in range(start + 1, len(lines)) if re.fullmatch(excluded[1], lines[i])), None)
        if end is None:
            raise PlanError(f"{path.name}: the block /{excluded[0]}/ ... /{excluded[1]}/ this planner excludes is gone;"
                            " update GROUPS in service_test_plan.py")
        skip = set(range(start, end + 1))
    found, index = [], 0
    while index < len(lines):
        first, text = index, lines[index]
        while True:
            if text.endswith("\\") and index + 1 < len(lines):
                index += 1
                text = text[:-1] + " " + lines[index]
                continue
            try:
                words = _tokens(text)
                break
            except ValueError:  # a quoted string continues on the next line
                if index + 1 < len(lines) and index - first < 60:
                    index += 1
                    text += "\n" + lines[index]
                    continue
                words = []
                break
        index += 1
        if first in skip:
            continue
        command: list[str] = []
        for position, word in enumerate(words + [";"]):
            if not command and words[position + 1:position + 2] == ["()"]:
                continue  # a function definition, not a call
            if word and set(word) <= set("();|&"):
                if command:
                    found.append((first + 1, command))
                command = []
            elif not command and (word in KEYWORDS or ASSIGNMENT.match(word)):
                continue
            else:
                command.append(word)
    return found


def _cargo_subcommand(words: list[str]) -> str:
    rest = [word for word in words[1:] if not word.startswith("+")]
    return "--version" if "--version" in words else (rest[0] if rest else "")


def _is_test_command(words: list[str]) -> bool:
    if words[0] == "run_required_rust_tests":
        return True
    return words[0] == "cargo" and _cargo_subcommand(words) not in (*CARGO_INFRA, "--version")


def _named_packages(words: list[str]) -> list[str]:
    packages = []
    for position, word in enumerate(words):
        if word in ("-p", "--package", "--test-package") and position + 1 < len(words):
            packages.append(words[position + 1])
        elif word.startswith(("--package=", "--test-package=")):
            packages.append(word.split("=", 1)[1])
    return packages


def fuzz_packages(root: Path) -> list[str]:
    manifest = tomllib.loads((root / "fuzz/Cargo.toml").read_text(encoding="utf-8"))
    names = []
    for value in manifest.get("dependencies", {}).values():
        if isinstance(value, dict) and "path" in value:
            dependency = (root / "fuzz" / value["path"] / "Cargo.toml").resolve()
            names.append(tomllib.loads(dependency.read_text(encoding="utf-8"))["package"]["name"])
    return sorted(names)


def script_packages(path: Path) -> list[str]:
    """Packages every test command of a helper script names (M3)."""
    return sorted({package for _, words in commands(path) if _is_test_command(words)
                   for package in _named_packages(words)})


def suites(group: str, root: Path = ROOT) -> list[Suite]:
    spec, scripts = GROUPS[group], root / "scripts/ci"
    errors, found = [], []
    for line, words in commands(scripts / spec.entry, spec.excluded):
        where = f"scripts/ci/{spec.entry}:{line}"
        text = " ".join(words)[:110]
        if words[0] not in spec.wrappers:
            if _is_test_command(words) and words[1:] != ["$@"]:
                errors.append(f"{where}: test command outside {' / '.join(spec.wrappers)}: {text}")
            continue
        command = words[1:]
        if command == ["$@"]:
            continue  # the wrapper forwarding its own arguments
        if words[0] == "pg_run_with_deadline":
            stage = command[0] if command else ""
            if stage in INFRA_STAGES or stage.startswith(INFRA_STAGE_PREFIXES):
                continue
            command = command[2:]
        packages = _named_packages(command)
        if command[:1] == ["cargo"] and _cargo_subcommand(command) == "fuzz":
            packages = fuzz_packages(root)
        helpers = [match for word in command for match in SHELL_SCRIPT.findall(word)]
        follows = next((key for key in FOLLOWS if key in words), None)
        if packages:
            found.append(Suite(spec.entry, line, text, tuple(sorted(set(packages))), "packages"))
        elif helpers and helpers[0] in SUBSTRATE_ONLY:
            found.append(Suite(spec.entry, line, text, (), "substrate", SUBSTRATE_ONLY[helpers[0]]))
        elif helpers and (named := script_packages(root / helpers[0])):
            found.append(Suite(spec.entry, line, text, tuple(named), "packages", f"via {helpers[0]}"))
        elif follows and found:
            found.append(Suite(spec.entry, line, text, found[-1].packages, "follows", FOLLOWS[follows]))
        else:
            errors.append(f"{where}: no package mapping: {text}")
    known = {tomllib.loads(path.read_text(encoding="utf-8"))["package"]["name"]
             for path in crate_manifests(root / "crates")}
    errors += [f"scripts/ci/{suite.file}:{suite.line}: unknown package {package}"
               for suite in found for package in suite.packages if package not in known]
    if errors:
        raise PlanError("\n".join(errors))
    return found


def group_files(group: str, scripts: Path) -> set[str]:
    """S2: the entry, its whole script closure and its callers, as repository paths."""
    entry = GROUPS[group].entry
    names = {entry, *generate_hooks.script_closure(entry, scripts), *CALLERS}
    return {f"scripts/ci/{name}" for name in names}


def named_strings(path: Path) -> str:
    """The text in which a group file can name a data file: a shell script whole;
    a Python module's string constants outside its own tests (a self-test that
    lists a path as a fixture does not make the group read it)."""
    if not path.is_file():
        return ""
    text = path.read_text(encoding="utf-8", errors="replace")
    if path.suffix != ".py":
        return text
    tests, module = set(), ast.parse(text)
    for node in ast.walk(module):
        if isinstance(node, (ast.FunctionDef, ast.ClassDef)) and re.match(r"(_?self_?test|test)|.*Tests$", node.name):
            tests |= {id(child) for child in ast.walk(node)}
    return "\n".join(node.value for node in ast.walk(module) if id(node) not in tests
                     and isinstance(node, ast.Constant) and isinstance(node.value, str))


def names_data(text: str, path: str) -> bool:
    """A full repository path, or a scripts/ci file's own name as a whole word."""
    name = path.removeprefix("scripts/ci/")
    return path in text or ("/" not in name and re.search(rf"(?<![\w.-]){re.escape(name)}(?![\w.-])", text) is not None)


def classify(group: str, changed: list[str], metadata_loader, root: Path = ROOT) -> tuple[str, list[str], str, list[str]]:
    """``(scope, affected packages, reason, explain lines)`` for one change set."""
    scripts = root / "scripts/ci"
    own = group_files(group, scripts)
    texts = [named_strings(scripts / path.removeprefix("scripts/ci/")) for path in sorted(own)]
    rust = rust_test_plan.rust_inputs()
    whole, built, lines = "", [], []
    for path in changed:
        if path in own:
            why = "group script -> whole group"
        elif path.startswith("scripts/") and not path.endswith((".sh", ".py")) and any(
                names_data(text, path) for text in texts):
            why = "data a group script names -> whole group"
        elif any(generate_hooks.glob_matches(glob, path) for glob in rust):
            why = "Rust build input -> owners and consumers (whole if unowned or a manifest)"
            built.append(path)
        else:
            why = "not read by this group -> ignored"
        if "whole group" in why and not whole:
            whole = f"{why.split(' ->')[0]} changed: {path}"
        lines.append(f"explain: {path}: {why}")
    if whole:
        return "whole", [], whole, lines
    if not built:
        return "none", [], "no input of this group changed", lines
    selected, reason = rust_test_plan.affected_packages(metadata_loader(), built, root)
    return ("packages" if reason == OWNED else "whole"), selected, reason, lines


def decide(found: list[Suite], scope: str, affected: list[str], reason: str) -> list[tuple[Suite, bool, str]]:
    rows: list[tuple[Suite, bool, str]] = []
    for suite in found:
        if scope != "packages":
            run, why = scope == "whole", reason
        elif suite.rule == "follows":
            run, why = rows[-1][1], f"follows {rows[-1][0].key}: {suite.note}"
        elif suite.rule == "substrate":
            run, why = False, f"runs with the whole group only: {suite.note}"
        else:
            hit = sorted(set(suite.packages) & set(affected))
            run = bool(hit)
            why = f"reached: {', '.join(hit)}" if run else f"not reached: {', '.join(suite.packages)}"
        rows.append((suite, run, why))
    return rows


def render(scope: str, reason: str, rows: list[tuple[Suite, bool, str]]) -> list[str]:
    selected = sum(run for _, run, _ in rows)
    lines = [f"scope={scope}", f"reason={reason}", f"selected={selected}/{len(rows)}"]
    lines += [f"{'run ' if run else 'skip'} {suite.key} {why} | {suite.text}" for suite, run, why in rows]
    return lines


def workspace_metadata() -> dict:
    environment = crate_test.environment(ROOT)
    # Every feature and platform, as the Rust gate scope does: an edge in any
    # graph a suite compiles must reach its consumer set.
    return json.loads(subprocess.check_output(crate_test.native_command(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"], environment),
        cwd=ROOT, env=environment))


def self_test() -> int:
    failures: list[str] = []

    def expect(condition: bool, label: str) -> None:
        if not condition:
            failures.append(label)

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        scripts = root / "scripts/ci"
        scripts.mkdir(parents=True)
        names = {"kernel": (), "store-a": ("kernel",), "store-b": (), "app": ("store-a",)}
        for name in names:
            manifest = root / "crates" / name / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(f'[package]\nname = "{name}"\n', encoding="utf-8")
        (root / "fuzz").mkdir()
        (root / "fuzz/Cargo.toml").write_text('[dependencies]\nk = { path = "../crates/kernel" }\n', encoding="utf-8")
        metadata = {"workspace_members": list(names),
                    "packages": [{"id": n, "name": n, "manifest_path": str(root / "crates" / n / "Cargo.toml")} for n in names],
                    "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in deps]} for n, deps in names.items()]}}
        (scripts / "object_store_tests.sh").write_text("run_required_rust_tests -p store-b --lib \\\n  -- --exact\n", encoding="utf-8")
        (scripts / "postgres_recovery_drill.sh").write_text("docker exec x psql\n", encoding="utf-8")
        (scripts / "check-all.sh").write_text("run postgres postgres scripts/ci/pg_tests.sh\n", encoding="utf-8")
        (scripts / "policy.json").write_text("{}\n", encoding="utf-8")
        (scripts / "unrelated.json").write_text("{}\n", encoding="utf-8")
        (scripts / "unrelated.py").write_text("\n", encoding="utf-8")
        entry = scripts / "pg_tests.sh"
        body = (
            "pg_required_test() {\n  run_required_rust_tests \"$@\"\n}\n"
            "self_test() {\n  pg_required_test --stub\n}\n"
            "pg_required_test -p store-a --features postgres \\\n  --require-case=a -- --test-threads=1 || record_failure \"$?\"\n"
            "pg_test cargo test -p store-b --features test-support || record_failure \"$?\"\n"
            "AWAKEN_CRASH_BACKEND=postgres pg_required_test -p app --locked || record_failure \"$?\"\n"
            "pg_run_with_deadline docker-info 30 docker info\n"
            "pg_run_with_deadline object-seed 30 python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --seed\n"
            "pg_run_with_deadline object-credential-rotation 30 \\\n  docker run --rm --entrypoint /bin/sh img -c \\\n"
            "  'mc alias set &&\n   mc admin user add'\n"
            "pg_run_with_deadline recovery-drill 900 \\\n  scripts/ci/postgres_recovery_drill.sh a b c\n"
            "docker run -v \"$PWD/scripts/ci/policy.json:/p\" img\n")
        entry.write_text(body, encoding="utf-8")
        loader = lambda: metadata  # noqa: E731

        def plan(changed: list[str]) -> tuple[str, dict[int, bool], list[str]]:
            scope, affected, reason, lines = classify("postgres", changed, loader, root)
            rows = decide(suites("postgres", root), scope, affected, reason)
            return scope, {suite.line: run for suite, run, _ in rows}, render(scope, reason, rows) + lines

        found = suites("postgres", root)
        expect([(s.line, s.packages, s.rule) for s in found] == [
            (7, ("store-a",), "packages"), (9, ("store-b",), "packages"), (10, ("app",), "packages"),
            (12, ("store-b",), "packages"), (13, ("store-b",), "follows"), (17, (), "substrate")],
            f"suite map: {[(s.line, s.packages, s.rule) for s in found]}")
        # T1 a doc-only (or unrelated-script) change selects nothing.
        scope, runs, _ = plan(["docs/design/a.md", "web/src/x.ts", "scripts/ci/unrelated.py", "scripts/ci/unrelated.json"])
        expect(scope == "none" and not any(runs.values()), "T1 doc-only selects nothing")
        # T2 one store crate selects its suites and its consumers' suites only.
        scope, runs, _ = plan(["crates/store-b/src/lib.rs"])
        expect(scope == "packages" and runs == {7: False, 9: True, 10: False, 12: True, 13: True, 17: False},
               f"T2 store-b selects only its suites: {runs}")
        scope, runs, _ = plan(["crates/kernel/src/lib.rs", "docs/x.md"])
        expect(runs == {7: True, 9: False, 10: True, 12: False, 13: False, 17: False}, f"T2 consumers: {runs}")
        # T3 lockfile, manifest, profile, group script, caller and named data select everything.
        for path in ("Cargo.lock", "crates/store-b/Cargo.toml", ".config/nextest.toml", "scripts/ci/pg_tests.sh",
                     "scripts/ci/object_store_tests.sh", "scripts/ci/check-all.sh", "scripts/ci/policy.json",
                     "fuzz/fuzz_targets/x.rs"):
            scope, runs, _ = plan([path, "crates/store-b/src/lib.rs"])
            expect(scope == "whole" and all(runs.values()), f"T3 {path} selects everything")
        # T5 explain names each suite, its verdict and why, and each path's class.
        _, _, lines = plan(["crates/store-b/src/lib.rs", "docs/a.md"])
        text = "\n".join(lines)
        for needle in ("scope=packages", "selected=3/6", "run  pg_tests.sh:9 reached: store-b",
                       "skip pg_tests.sh:7 not reached: store-a", "run  pg_tests.sh:13 follows pg_tests.sh:12",
                       "skip pg_tests.sh:17 runs with the whole group only",
                       "explain: docs/a.md: not read by this group -> ignored"):
            expect(needle in text, f"T5 explain lacks {needle!r}")
        # T4 an unmapped suite, an unknown package or stage, or an unwrapped test is a hard error.
        for broken, label in (("pg_required_test --lib only\n", "no package mapping"),
                              ("pg_test cargo test -p ghost\n", "unknown package ghost"),
                              ("pg_run_with_deadline brand-new-stage 30 docker run x\n", "no package mapping"),
                              ("cargo test -p store-a\n", "test command outside"),
                              ("RUSTFLAGS=x run_required_rust_tests -p store-a\n", "test command outside")):
            entry.write_text(body + broken, encoding="utf-8")
            try:
                suites("postgres", root)
                expect(False, f"T4 accepted {broken.strip()!r}")
            except PlanError as error:
                expect(label in str(error) and ":20:" in str(error), f"T4 {broken.strip()!r}: {error}")
        entry.write_text(body.replace("self_test() {", "selftest() {"), encoding="utf-8")
        try:
            suites("postgres", root)
            expect(False, "T4 a vanished excluded block must fail")
        except PlanError:
            pass
        # M2 the fuzz runner maps to the fuzz crate's path dependencies.
        (scripts / "check-reliability.sh").write_text(
            'if [ "${1:-}" = "--session-observations" ]; then\n  cargo test -p kernel\nfi\n'
            "(cd fuzz && affected_suite cargo +nightly fuzz run t -- -max_total_time=1)\n"
            "(cd fuzz && cargo +nightly metadata --locked)\ncargo clean -p kernel\n"
            'RUSTFLAGS="-Zsanitizer=thread" affected_suite cargo +nightly test -p store-b 2>&1 | tee log\n'
            'affected_suite python3 scripts/ci/check_reliability.py --tsan-log "$log"\n', encoding="utf-8")
        reliability = suites("reliability", root)
        expect([(s.line, s.packages, s.rule) for s in reliability]
               == [(4, ("kernel",), "packages"), (7, ("store-b",), "packages"), (8, ("store-b",), "follows")],
               f"M2 reliability map: {[(s.line, s.packages, s.rule) for s in reliability]}")
        # R1 the shell helper runs only what the planner selected; an unmapped line stops the run.
        failures += _helper_test(root)
    # The real groups: every suite maps to an existing workspace package.
    try:
        postgres, reliability = suites("postgres"), suites("reliability")
        expect(len(postgres) >= 45 and len(reliability) >= 20, f"real suite counts {len(postgres)}/{len(reliability)}")
        expect(any(s.note.startswith("via scripts/ci/object_store_tests.sh") for s in postgres), "object stages map via M3")
    except PlanError as error:
        failures.append(f"real groups: {error}")
    pg_text = (SCRIPTS / "pg_tests.sh").read_text(encoding="utf-8")
    for function in ("pg_required_test", "pg_test", "pg_run_with_deadline"):
        body = pg_text.split(f"\n{function}() {{\n", 1)[-1].split("\n}\n", 1)[0]
        expect("affected_skip_" in body, f"{function} does not consult the affected decision")
    caller = (SCRIPTS / "check-all.sh").read_text(encoding="utf-8")
    expect('[ "$full_gate" -eq 0 ]' in caller.split("AWAKEN_SERVICE_AFFECTED_BASE", 1)[-1][:200],
           "check-all.sh must never narrow --full-gate")
    for failure in failures:
        print(f"service_test_plan self-test: FAIL {failure}", file=sys.stderr)
    if not failures:
        print("service_test_plan self-test: S1-S4, M1-M5, T1-T5 and the shell helper hold")
    return 1 if failures else 0


def _helper_test(root: Path) -> list[str]:
    """Run the real _affected_suites.sh against a canned plan."""
    scripts = root / "scripts/ci"
    (scripts / "_affected_suites.sh").write_bytes((SCRIPTS / "_affected_suites.sh").read_bytes())
    (scripts / "service_test_plan.py").write_text(
        "print('scope=packages')\nprint('selected=1/2')\n"
        "print('run  probe.sh:5 reached: a | x')\nprint('skip probe.sh:6 not reached: b | y')\n", encoding="utf-8")
    (scripts / "probe.sh").write_text(
        "set -euo pipefail\nsource scripts/ci/_affected_suites.sh\n"
        'affected_suites_load postgres HEAD >/dev/null || exit $?\n'
        "touch line4\n"
        "affected_suite touch ran\naffected_suite touch skipped\n"
        'affected_selected_after 6 && touch later || touch none-later\n'
        "affected_suite touch unmapped\n", encoding="utf-8", newline="\n")
    failures = []
    command = [sys.executable, str(SCRIPTS / "_native_shell.py"), "scripts/ci/probe.sh"]
    environment = dict(os.environ)
    environment.pop("AWAKEN_BUILD_WORKLOAD_KIND", None)
    result = subprocess.run(command, cwd=root, capture_output=True, text=True, env=environment)
    made = {path.name for path in root.iterdir() if path.is_file()}
    if result.returncode != 2 or made != {"line4", "ran", "none-later"} or "no affected decision" not in result.stderr:
        failures.append(f"R1 helper: exit {result.returncode}, files {sorted(made)}, {result.stderr.strip()}")
    result = subprocess.run(command, cwd=root, capture_output=True, text=True,
                            env={**environment, "AWAKEN_BUILD_WORKLOAD_KIND": "qualification"})
    if result.returncode != 2 or "never narrowed" not in result.stderr:
        failures.append(f"R2 a qualification run is never narrowed: exit {result.returncode}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__.splitlines()[0])
    parser.add_argument("--group", choices=sorted(GROUPS))
    parser.add_argument("--base", help="revision the change is measured from (index, worktree and untracked included)")
    parser.add_argument("--explain", action="store_true", help="also print why each changed path counts (stderr)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.group or args.base:
            parser.error("--self-test is a standalone mode")
        return self_test()
    if not args.group or not args.base:
        parser.error("--group and --base are required")
    try:
        found = suites(args.group)
        base = rust_test_plan.git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
        changed = rust_test_plan.changed_sources(base)
        scope, affected, reason, lines = classify(args.group, changed, workspace_metadata)
    except PlanError as error:
        print(f"{error}\n{RULE}\n{WHY}\nfix: name the package with -p, or declare the command in "
              "FOLLOWS/SUBSTRATE_ONLY/INFRA_STAGES of scripts/ci/service_test_plan.py", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as error:
        print(f"Cannot plan the affected {args.group} group (status {error.returncode}): {error.cmd}", file=sys.stderr)
        return error.returncode if error.returncode > 0 else 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot plan the affected {args.group} group: {error}", file=sys.stderr)
        return 1
    if args.explain:
        print("\n".join(lines), file=sys.stderr)
    print("\n".join(render(scope, reason, decide(found, scope, affected, reason))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
