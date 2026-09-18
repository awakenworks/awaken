#!/usr/bin/env python3
"""Refuse a guard whose scope is hand-written instead of derived from its subject.

Five defects in `main` came from one shape, and the shape is not in the product
at all -- it is in the checkers. A rule states a subject ("every recovery-path
file", "every test that waits", "every file the formal fixture executes") and
then states its *scope* a second time, as a literal list of paths, globs or
suffixes. The two drift, always in the same direction: the subject grows, the
list does not, and the rule goes on reporting OK over a corpus it no longer
covers. Nothing fails, because a scope that is too narrow has no symptom.

What it cost, in the order it was found:

* `scripts/ci/test_formal_run_evidence.py` copied a hand-listed file set into
  its fixture. `check_formal.sh` grew a dependency on `gate_tools.py`; the list
  did not, and the formal gate failed inside its own self-test.
* `check_test_waits.py` policed "a wait must settle on a durable fact" over
  `*.rs` and `*.mjs` only. Fourteen shell suites and the console's Playwright
  specs were never read; two of them fell out of a readiness poll and ran the
  load profile against a port nothing was listening on.
* `check_mcp_tool_identity_is_owned` scanned Rust only, while the console
  re-implemented the `mcp__` prefix parse in `ToolOverridesEditor.tsx`.
* `check_observation_collapse.py` held a nine-path `RECOVERY_PATH` tuple; the
  files that actually read checkpoints had become thirty-one.
* `check_test_orchestration.py` holds `PUSH_GATE_COMMANDS` as a literal tuple.

Each of those was fixed by deriving the scope from the subject -- a dependency
closure, an `INPUT_GLOBS` declaration, a directory walk, workspace metadata.
This file is the rule that stops the shape from coming back: the checkers are
themselves a corpus, and a *new* hand-written scope list among them is refused.

Two rules, each ratcheted, each with a registry whose entries must say why:

W1  A module-level literal collection of two or more repository paths, path
    globs or file suffixes is a hand-written scope list. `INPUT_GLOBS` is the
    one exception, because it is the declaration everything else derives from:
    `generate_hooks.py` renders the lefthook trigger from it and a checker
    walks it to build its corpus. Every other such list is registered in
    `scan_scope_registry.json` with the reason it cannot be derived.

W2  Every entry point under `scripts/ci` that walks the repository declares
    `INPUT_GLOBS`, or is registered with the shell gate that runs it -- and the
    registry is checked: the named runner must really invoke the script, so a
    registration cannot go stale without failing.

Both registries are complete, not advisory: an entry naming something that no
longer exists is an error, so ground gained is held rather than quietly lost.
"""

from __future__ import annotations

import argparse
import ast
import json
import re
import sys
import tempfile
from pathlib import Path

# This checker reads every `scripts/ci` module, its own registry, and the shell
# gates a registration may name.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/*.py",
    "scripts/ci/*.sh",
    "scripts/ci/scan_scope_registry.json",
)

ROOT = Path(__file__).resolve().parents[2]
REGISTRY = Path(__file__).resolve().parent / "scan_scope_registry.json"

# The one literal scope allowed to be literal: it is the single declaration the
# trigger renderer and the checker's own corpus walk both read.
DECLARATION = "INPUT_GLOBS"

# A repository path or path glob: at least one separator, or a `**` wildcard.
PATH_LITERAL = re.compile(r"^[A-Za-z0-9_.*][A-Za-z0-9_.*{}\[\],-]*(?:/[A-Za-z0-9_.*{}\[\],-]+)+$")
# A file suffix, the other way a scope is written down: `.rs`, `*.mjs`.
SUFFIX_LITERAL = re.compile(r"^\*?\.[A-Za-z0-9]{1,6}$")
# A repository walk: `.glob(`/`.rglob(` anywhere in the module text.
WALKS_THE_REPOSITORY = re.compile(r"\.r?glob\s*\(")
ENTRY_POINT = re.compile(r'^if __name__ == ["\']__main__["\']:', re.MULTILINE)
DECLARES = re.compile(r"^INPUT_GLOBS\b", re.MULTILINE)

MIN_REASON = 30


def literal_strings(node: ast.expr) -> list[str]:
    """The string literals of a wholly-literal collection, else `[]`.

    A collection built from a comprehension, a call or another name is derived
    by construction -- there is nothing hand-written to police -- so only a
    tuple/list/set/dict of constants, or `frozenset(...)` of one, is returned.
    """

    found: list[str] = []
    if isinstance(node, (ast.Tuple, ast.List, ast.Set)):
        for element in node.elts:
            if not (isinstance(element, ast.Constant) and isinstance(element.value, str)):
                return []
            found.append(element.value)
    elif isinstance(node, ast.Dict):
        for key in node.keys:
            if not (isinstance(key, ast.Constant) and isinstance(key.value, str)):
                return []
            found.append(key.value)
    elif (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id in {"frozenset", "set", "tuple", "list"}
    ):
        for argument in node.args:
            found.extend(literal_strings(argument))
    return found


def is_scope(values: list[str]) -> bool:
    """Do these literals *name a scope* rather than describe a vocabulary?

    Two or more entries, and every one of them a path, a glob or a suffix. A
    list that mixes paths with prose, method names or shell commands is saying
    something else; requiring all of them keeps the rule about scope.
    """

    if len(values) < 2:
        return False
    return all(PATH_LITERAL.match(value) or SUFFIX_LITERAL.match(value) for value in values)


def scope_lists(module: Path) -> list[tuple[str, int, int]]:
    """Every hand-written scope list in `module`: (name, line, size)."""

    tree = ast.parse(module.read_text(encoding="utf-8"))
    found: list[tuple[str, int, int]] = []
    for statement in tree.body:
        if isinstance(statement, ast.Assign) and len(statement.targets) == 1 and isinstance(
            statement.targets[0], ast.Name
        ):
            name, value = statement.targets[0].id, statement.value
        elif isinstance(statement, ast.AnnAssign) and isinstance(statement.target, ast.Name):
            if statement.value is None:
                continue
            name, value = statement.target.id, statement.value
        else:
            continue
        if name == DECLARATION:
            continue
        values = literal_strings(value)
        if is_scope(values):
            found.append((name, statement.lineno, len(values)))
    return found


def undeclared_scanners(scripts: Path) -> list[str]:
    """Entry points under `scripts` that walk the repository without a declaration."""

    found: list[str] = []
    for module in sorted(scripts.glob("*.py")):
        source = module.read_text(encoding="utf-8")
        if not ENTRY_POINT.search(source) or not WALKS_THE_REPOSITORY.search(source):
            continue
        if DECLARES.search(source):
            continue
        found.append(module.name)
    return found


def load_registry(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def _reason_errors(label: str, key: str, entry: dict) -> list[str]:
    reason = entry.get("reason", "")
    if len(reason) < MIN_REASON:
        return [
            f"{label} entry {key} must say why the scope cannot be derived, in at "
            f"least {MIN_REASON} characters"
        ]
    return []


def validation_errors(root: Path, registry: dict) -> list[str]:
    """Every W1/W2 violation, plus every stale or unreasoned registry entry."""

    scripts = root / "scripts" / "ci"
    errors: list[str] = []

    # ---- W1: hand-written scope lists -----------------------------------
    registered_lists: dict[str, dict] = registry.get("scope_lists", {})
    live: dict[str, tuple[int, int]] = {}
    for module in sorted(scripts.glob("*.py")):
        for name, line, size in scope_lists(module):
            live[f"{module.name}:{name}"] = (line, size)

    for key, (line, size) in sorted(live.items()):
        module, _, name = key.partition(":")
        if key not in registered_lists:
            errors.append(
                f"scripts/ci/{module}:{line}: {name} is a hand-written scope list "
                f"({size} literal paths). Derive it from its subject -- workspace "
                "metadata, a directory walk, a dependency closure, or this module's "
                f"{DECLARATION} declaration -- or register it in "
                f"{REGISTRY.name} with the reason it cannot be derived."
            )
            continue
        errors.extend(_reason_errors("scope_lists", key, registered_lists[key]))

    for key in sorted(registered_lists):
        if key not in live:
            errors.append(
                f"scope_lists names {key}, which no longer exists; drop the stale "
                "entry so the ratchet records the ground gained"
            )

    baseline = registry.get("baseline", {}).get("scope_lists")
    if baseline is None:
        errors.append("the registry must carry a baseline count for scope_lists")
    elif len(live) > baseline:
        errors.append(
            f"{len(live)} hand-written scope lists, baseline {baseline}: a new one "
            "was added. The baseline only goes down."
        )
    elif len(live) < baseline:
        errors.append(
            f"{len(live)} hand-written scope lists, baseline {baseline}: lower the "
            "baseline to hold the ground gained."
        )

    # ---- W2: entry points that scan without declaring their inputs -------
    registered_scanners: dict[str, dict] = registry.get("scanners_without_input_globs", {})
    undeclared = undeclared_scanners(scripts)
    for name in undeclared:
        entry = registered_scanners.get(name)
        if entry is None:
            errors.append(
                f"scripts/ci/{name} walks the repository from its own entry point but "
                f"declares no {DECLARATION}, so its scope is stated only inside its "
                "code and no hook trigger can be rendered from it. Declare "
                f"{DECLARATION}, or register it in {REGISTRY.name} with the gate that "
                "runs it."
            )
            continue
        errors.extend(_reason_errors("scanners_without_input_globs", name, entry))
        runner = entry.get("runner")
        if runner is None:
            continue
        gate = root / runner
        if not gate.is_file():
            errors.append(
                f"scanners_without_input_globs names runner {runner} for {name}, "
                "which does not exist"
            )
        elif name not in gate.read_text(encoding="utf-8"):
            errors.append(
                f"scanners_without_input_globs claims {runner} runs {name}, but "
                f"{runner} does not name it; the registration is stale"
            )

    for name in sorted(registered_scanners):
        if name not in undeclared:
            errors.append(
                f"scanners_without_input_globs names {name}, which now declares "
                f"{DECLARATION} or no longer scans; drop the stale entry"
            )

    scanner_baseline = registry.get("baseline", {}).get("scanners_without_input_globs")
    if scanner_baseline is None:
        errors.append("the registry must carry a baseline count for scanners_without_input_globs")
    elif len(undeclared) > scanner_baseline:
        errors.append(
            f"{len(undeclared)} entry points scan without declaring {DECLARATION}, "
            f"baseline {scanner_baseline}: a new one was added. The baseline only goes down."
        )
    elif len(undeclared) < scanner_baseline:
        errors.append(
            f"{len(undeclared)} entry points scan without declaring {DECLARATION}, "
            f"baseline {scanner_baseline}: lower the baseline to hold the ground gained."
        )

    return errors


def selftest() -> None:
    """Decision table for W1 and W2, over every shape `scripts/ci` holds.

    | Rule | Cause                                                      | Effect |
    |------|------------------------------------------------------------|--------|
    | M1   | a tuple of two repository paths                             | scope  |
    | M2   | a tuple of file suffixes                                    | scope  |
    | M3   | a dict keyed by repository paths                            | scope  |
    | M4   | `frozenset({...})` of paths                                 | scope  |
    | M5   | a single path (an owner anchor, not a list)                 | not    |
    | M6   | method names, shell commands, prose                         | not    |
    | M7   | paths mixed with non-paths                                  | not    |
    | M8   | a list built by a comprehension or a call                   | not    |
    | M9   | the `INPUT_GLOBS` declaration itself                         | not    |
    | M10  | a scope list inside a function, not module level            | not    |
    | M11  | a new unregistered scope list in a scratch checker          | reject |
    | M12  | the same list, registered with a reason                     | accept |
    | M13  | the same list, registered with a one-word reason            | reject |
    | M14  | a registry entry for a list that is gone                    | reject |
    | M15  | one more scope list than the baseline                       | reject |
    | M16  | one fewer than the baseline (ground gained, unrecorded)     | reject |
    | M17  | an entry point that globs and declares no INPUT_GLOBS       | reject |
    | M18  | the same, registered with a runner that really runs it      | accept |
    | M19  | the same, registered with a runner that does not name it    | reject |
    | M20  | a module that globs but is not an entry point (a helper)    | not    |
    | M21  | an entry point that globs and declares INPUT_GLOBS          | accept |
    """

    def parsed(source: str) -> list[tuple[str, int, int]]:
        with tempfile.TemporaryDirectory() as directory:
            module = Path(directory) / "probe.py"
            module.write_text(source, encoding="utf-8")
            return scope_lists(module)

    names = lambda source: {name for name, _, _ in parsed(source)}  # noqa: E731

    assert names('A = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n') == {"A"}, "M1"
    assert names('A = (".rs", ".mjs", ".sh")\n') == {"A"}, "M2"
    assert names('A = {"crates/a/x.rs": "why", "crates/b/y.rs": "why"}\n') == {"A"}, "M3"
    assert names('A = frozenset({"crates/a/x.rs", "crates/b/y.rs"})\n') == {"A"}, "M4"
    assert names('A = ("crates/a/src/lib.rs",)\n') == set(), "M5"
    assert names('A = ("unwrap_or_default", "ok_or_else")\n') == set(), "M6"
    assert names('A = ("crates/a/x.rs", "cargo test --workspace")\n') == set(), "M7"
    assert names('A = tuple(p for p in roots)\n') == set(), "M8"
    assert names('INPUT_GLOBS = ("crates/**/*.rs", "e2e/**/*.mjs")\n') == set(), "M9"
    assert names('def f():\n    A = ("crates/a/x.rs", "crates/b/y.rs")\n') == set(), "M10"

    # W1/W2 end to end, against a scratch tree that is a miniature scripts/ci.
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        scripts = root / "scripts" / "ci"
        scripts.mkdir(parents=True)
        (root / "scripts" / "ci" / "run-gate.sh").write_text(
            "python3 scripts/ci/check_probe.py\n", encoding="utf-8"
        )
        clean = {
            "scope_lists": {},
            "scanners_without_input_globs": {},
            "baseline": {"scope_lists": 0, "scanners_without_input_globs": 0},
        }
        assert validation_errors(root, clean) == [], "an empty corpus is clean"

        probe = scripts / "check_probe.py"
        probe.write_text(
            'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
            'INPUT_GLOBS = ("crates/**/*.rs",)\n'
            'list(ROOT.glob("crates/**/*.rs"))\n'
            'if __name__ == "__main__":\n    pass\n',
            encoding="utf-8",
        )
        one = dict(clean, baseline={"scope_lists": 1, "scanners_without_input_globs": 0})
        assert any("hand-written scope list" in error for error in
                   validation_errors(root, one)), "M11"

        reason = "the eight route owners are named by ADR, not discoverable from the tree"
        registered = {
            "scope_lists": {"check_probe.py:OWNERS": {"reason": reason}},
            "scanners_without_input_globs": {},
            "baseline": {"scope_lists": 1, "scanners_without_input_globs": 0},
        }
        assert validation_errors(root, registered) == [], "M12"

        short = json.loads(json.dumps(registered))
        short["scope_lists"]["check_probe.py:OWNERS"]["reason"] = "legacy"
        assert any("must say why" in error for error in
                   validation_errors(root, short)), "M13"

        stale = json.loads(json.dumps(registered))
        stale["scope_lists"]["check_probe.py:GONE"] = {"reason": reason}
        stale["baseline"]["scope_lists"] = 1
        assert any("no longer exists" in error for error in
                   validation_errors(root, stale)), "M14"

        lowered = json.loads(json.dumps(registered))
        lowered["baseline"]["scope_lists"] = 0
        assert any("only goes down" in error for error in
                   validation_errors(root, lowered)), "M15"

        raised = json.loads(json.dumps(registered))
        raised["baseline"]["scope_lists"] = 2
        assert any("hold the ground gained" in error for error in
                   validation_errors(root, raised)), "M16"
        assert validation_errors(root, registered) == [], "M21"

        # W2: the same module, now scanning without a declaration.
        probe.write_text(
            'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
            'list(ROOT.glob("crates/**/*.rs"))\n'
            'if __name__ == "__main__":\n    pass\n',
            encoding="utf-8",
        )
        undeclared = json.loads(json.dumps(registered))
        assert any("declares no INPUT_GLOBS" in error for error in
                   validation_errors(root, undeclared)), "M17"

        runner_reason = "runs inside the rust shell gate, which has no per-file trigger"
        wired = json.loads(json.dumps(registered))
        wired["scanners_without_input_globs"]["check_probe.py"] = {
            "runner": "scripts/ci/run-gate.sh",
            "reason": runner_reason,
        }
        wired["baseline"]["scanners_without_input_globs"] = 1
        assert validation_errors(root, wired) == [], "M18"

        wrong = json.loads(json.dumps(wired))
        (root / "scripts" / "ci" / "other-gate.sh").write_text("true\n", encoding="utf-8")
        wrong["scanners_without_input_globs"]["check_probe.py"]["runner"] = (
            "scripts/ci/other-gate.sh"
        )
        assert any("does not name it" in error for error in
                   validation_errors(root, wrong)), "M19"

        # M20: a helper module that globs but has no entry point is not a scanner.
        probe.write_text(
            'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
            'list(ROOT.glob("crates/**/*.rs"))\n',
            encoding="utf-8",
        )
        helper = json.loads(json.dumps(registered))
        assert validation_errors(root, helper) == [], "M20"

    # Corpus assertion: the rule holds over the *set* of checkers, so a checker
    # added tomorrow inherits it without anyone remembering to extend anything.
    registry = load_registry(REGISTRY)
    scripts = ROOT / "scripts" / "ci"
    modules = sorted(scripts.glob("*.py"))
    assert modules, "the checker corpus must not be empty"
    for module in modules:
        for name, _, _ in scope_lists(module):
            assert f"{module.name}:{name}" in registry["scope_lists"], (
                f"{module.name}:{name} is a scope list with no registry entry"
            )
    for name in undeclared_scanners(scripts):
        assert name in registry["scanners_without_input_globs"], (
            f"{name} scans without declaring {DECLARATION} and has no registry entry"
        )
    print("check_scan_scope selftest: the decision table holds")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        selftest()
        return 0

    registry = load_registry(REGISTRY)
    errors = validation_errors(arguments.root, registry)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        print(
            "\nA guard whose scope is written by hand is a guard that will one day "
            "report OK over code it no longer reads. Derive the scope from the "
            "subject, or state in the registry why it cannot be.",
            file=sys.stderr,
        )
        return 1
    counted = sum(
        len(scope_lists(module)) for module in sorted((arguments.root / "scripts/ci").glob("*.py"))
    )
    print(
        f"OK - {counted} registered scope lists and "
        f"{len(undeclared_scanners(arguments.root / 'scripts/ci'))} registered "
        "undeclared scanners, none new."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
