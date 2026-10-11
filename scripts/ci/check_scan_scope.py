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

W1: A module-level literal collection of two or more repository paths, path
    globs or file suffixes is a hand-written scope list. `INPUT_GLOBS` is the
    one exception, because it is the declaration everything else derives from:
    `generate_hooks.py` renders the lefthook trigger from it and a checker
    walks it to build its corpus. Every other such list is registered in
    `scan_scope_registry.json` with the reason it cannot be derived.

W2: Every entry point under `scripts/ci` that walks the repository declares
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

import _architecture_ratchet

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
# Direct walks retain the original detection; delegated walks use the actual
# imported source owner below, so removing a copied walk does not hide a scanner.
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


def _module_sources(scripts: Path) -> dict[Path, tuple[str, ast.Module]]:
    """The one working-text/AST acquisition for this owner's module corpus."""
    modules = {}
    for module in sorted(scripts.glob("*.py")):
        source = module.read_text(encoding="utf-8")
        modules[module] = (source, ast.parse(source))
    return modules


def scope_lists(tree: ast.Module) -> list[tuple[str, int, int]]:
    """Every hand-written scope list in the selected AST: (name, line, size)."""

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


def walks_repository(source: str, tree: ast.Module) -> bool:
    """Direct walks or calls to the existing canonical tracked-file reader."""
    if WALKS_THE_REPOSITORY.search(source):
        return True
    modules: set[str] = set()
    readers: set[str] = set()
    for statement in tree.body:
        if isinstance(statement, ast.Import):
            modules.update(alias.asname or alias.name for alias in statement.names
                           if alias.name == "formal_sources")
        elif isinstance(statement, ast.ImportFrom) and statement.module == "formal_sources":
            readers.update(alias.asname or alias.name for alias in statement.names
                           if alias.name == "tracked_files")
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        function = node.func
        if isinstance(function, ast.Name) and function.id in readers:
            return True
        if (isinstance(function, ast.Attribute) and function.attr == "tracked_files"
                and isinstance(function.value, ast.Name) and function.value.id in modules):
            return True
    return False


def undeclared_scanners(modules: dict[Path, tuple[str, ast.Module]]) -> list[str]:
    """Selected entry points that walk the repository without a declaration."""

    found: list[str] = []
    for module, (source, tree) in modules.items():
        if not ENTRY_POINT.search(source) or not walks_repository(source, tree):
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


def validation_errors(
    root: Path, registry: dict
) -> tuple[list[str], dict[str, tuple[int, int]], list[str]]:
    """Every W1/W2 violation and the exact facts checked for its report."""

    scripts = root / "scripts" / "ci"
    errors: list[str] = []

    # ---- W1: hand-written scope lists -----------------------------------
    registered_lists: dict[str, dict] = registry.get("scope_lists", {})
    modules = _module_sources(scripts)
    live: dict[str, tuple[int, int]] = {}
    for module, (_, tree) in modules.items():
        for name, line, size in scope_lists(tree):
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
    else:
        errors += _architecture_ratchet.compare(
            (_architecture_ratchet.Rule("W1", "derive the list from its subject; the baseline only goes down"),),
            {"W1": {"scope_lists": len(live)}}, {}, {"W1": {"scope_lists": baseline}}, REGISTRY.name,
            lower_advice="lower scripts/ci/scan_scope_registry.json baseline.scope_lists in this commit "
                         "to hold the ground gained",
        )

    # ---- W2: entry points that scan without declaring their inputs -------
    registered_scanners: dict[str, dict] = registry.get("scanners_without_input_globs", {})
    undeclared = undeclared_scanners(modules)
    runner_sources: dict[Path, str | None] = {}
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
        if gate not in runner_sources:
            runner_sources[gate] = gate.read_text(encoding="utf-8") if gate.is_file() else None
        text = runner_sources[gate]
        if text is None:
            errors.append(
                f"scanners_without_input_globs names runner {runner} for {name}, "
                "which does not exist"
            )
        elif name not in text:
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
    else:
        errors += _architecture_ratchet.compare(
            (_architecture_ratchet.Rule("W2", "declare INPUT_GLOBS; the baseline only goes down"),),
            {"W2": {"scanners_without_input_globs": len(undeclared)}}, {},
            {"W2": {"scanners_without_input_globs": scanner_baseline}}, REGISTRY.name,
            lower_advice="lower scripts/ci/scan_scope_registry.json baseline.scanners_without_input_globs "
                         "in this commit to hold the ground gained",
        )

    return errors, live, undeclared


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

    names = lambda source: {name for name, _, _ in scope_lists(ast.parse(source))}  # noqa: E731

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
        assert validation_errors(root, clean)[0] == [], "an empty corpus is clean"

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
                   validation_errors(root, one)[0]), "M11"

        reason = "the eight route owners are named by ADR, not discoverable from the tree"
        registered = {
            "scope_lists": {"check_probe.py:OWNERS": {"reason": reason}},
            "scanners_without_input_globs": {},
            "baseline": {"scope_lists": 1, "scanners_without_input_globs": 0},
        }
        assert validation_errors(root, registered)[0] == [], "M12"

        short = json.loads(json.dumps(registered))
        short["scope_lists"]["check_probe.py:OWNERS"]["reason"] = "legacy"
        assert any("must say why" in error for error in
                   validation_errors(root, short)[0]), "M13"

        stale = json.loads(json.dumps(registered))
        stale["scope_lists"]["check_probe.py:GONE"] = {"reason": reason}
        stale["baseline"]["scope_lists"] = 1
        assert any("no longer exists" in error for error in
                   validation_errors(root, stale)[0]), "M14"

        lowered = json.loads(json.dumps(registered))
        lowered["baseline"]["scope_lists"] = 0
        assert any("only goes down" in error for error in
                   validation_errors(root, lowered)[0]), "M15"

        raised = json.loads(json.dumps(registered))
        raised["baseline"]["scope_lists"] = 2
        assert any("hold the ground gained" in error for error in
                   validation_errors(root, raised)[0]), "M16"
        assert validation_errors(root, registered)[0] == [], "M21"

        # W2: the same module, now scanning without a declaration.
        probe.write_text(
            'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
            'list(ROOT.glob("crates/**/*.rs"))\n'
            'if __name__ == "__main__":\n    pass\n',
            encoding="utf-8",
        )
        undeclared = json.loads(json.dumps(registered))
        assert any("declares no INPUT_GLOBS" in error for error in
                   validation_errors(root, undeclared)[0]), "M17"

        runner_reason = "runs inside the rust shell gate, which has no per-file trigger"
        wired = json.loads(json.dumps(registered))
        wired["scanners_without_input_globs"]["check_probe.py"] = {
            "runner": "scripts/ci/run-gate.sh",
            "reason": runner_reason,
        }
        wired["baseline"]["scanners_without_input_globs"] = 1
        assert validation_errors(root, wired)[0] == [], "M18"

        wrong = json.loads(json.dumps(wired))
        (root / "scripts" / "ci" / "other-gate.sh").write_text("true\n", encoding="utf-8")
        wrong["scanners_without_input_globs"]["check_probe.py"]["runner"] = (
            "scripts/ci/other-gate.sh"
        )
        assert any("does not name it" in error for error in
                   validation_errors(root, wrong)[0]), "M19"

        # M22: the same entry point delegates its unchanged repository scan to
        # formal_sources instead of copying rglob. The original declaration/
        # registry/runner rules still apply; consolidation is not ground gained.
        # M23: comments/strings, unbound names and another owner's same method
        # name are not that call. Recognize imports and AST calls, not mentions.
        for imported, call in (
            ('import formal_sources', 'formal_sources.tracked_files(ROOT)'),
            ('import formal_sources as sources', 'sources.tracked_files(ROOT)'),
            ('from formal_sources import tracked_files as files', 'files(ROOT)'),
        ):
            source = (
                'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
                f'{imported}\n{call}\n'
                'if __name__ == "__main__":\n    pass\n'
            )
            probe.write_text(source, encoding="utf-8")
            assert validation_errors(root, wired)[0] == [], "M22 registered delegated scan"
            assert any("declares no INPUT_GLOBS" in error for error in
                       validation_errors(root, registered)[0]), "M22 declaration still required"
            source = 'INPUT_GLOBS = ("Cargo.toml",)\n' + source
            probe.write_text(source, encoding="utf-8")
            assert undeclared_scanners({probe: (source, ast.parse(source))}) == [], "M22 declared delegation"
        for source in (
            'import formal_sources\n# formal_sources.tracked_files(ROOT)\n',
            'import formal_sources\ntext = "formal_sources.tracked_files(ROOT)"\n',
            'formal_sources.tracked_files(ROOT)\n',
            'import other\nother.tracked_files(ROOT)\n',
        ):
            source += 'if __name__ == "__main__":\n    pass\n'
            probe.write_text(source, encoding="utf-8")
            assert undeclared_scanners({probe: (source, ast.parse(source))}) == [], "M23 no unrelated or textual call"

        # M20: a helper module that globs but has no entry point is not a scanner.
        probe.write_text(
            'OWNERS = ("crates/a/src/lib.rs", "crates/b/src/lib.rs")\n'
            'list(ROOT.glob("crates/**/*.rs"))\n',
            encoding="utf-8",
        )
        helper = json.loads(json.dumps(registered))
        assert validation_errors(root, helper)[0] == [], "M20"

    # Corpus assertion: the rule holds over the *set* of checkers, so a checker
    # added tomorrow inherits it without anyone remembering to extend anything.
    registry = load_registry(REGISTRY)
    scripts = ROOT / "scripts" / "ci"
    modules = _module_sources(scripts)
    assert modules, "the checker corpus must not be empty"
    for module, (_, tree) in modules.items():
        for name, _, _ in scope_lists(tree):
            assert f"{module.name}:{name}" in registry["scope_lists"], (
                f"{module.name}:{name} is a scope list with no registry entry"
            )
    for name in undeclared_scanners(modules):
        assert name in registry["scanners_without_input_globs"], (
            f"{name} scans without declaring {DECLARATION} and has no registry entry"
        )
    _selftest_acquisition()
    print("check_scan_scope selftest: the decision table holds")


def _selftest_acquisition() -> None:
    """Check admission and reporting against the same actual file acquisition."""
    import contextlib
    import io
    from collections import Counter
    from itertools import product
    from unittest.mock import patch

    # M24-M30 graph: valid argv -> registry -> first module text/AST -> W1 + W2
    # -> independent member/reason/runner/count decisions -> same-fact report.
    # A second read cannot replace W2 or the report's subject. Each live entry
    # still makes its own decision when entries share one runner path.
    #
    # | Row | cause                                      | effect |
    # | M24 | two selectors/report, shared runner         | module read/parse1; runner read1 |
    # | M25a| bad first scanner, clean text on reread     | first scanner rejected, no reread |
    # | M25b| valid first list, extra list on reread      | report first list count1 |
    # | M26 | below/equal/above/missing count × hard rule | only equal + all hard rules admit |
    # | M27 | runner None/name/no-name/missing            | admit/admit/reject/reject |
    # | M28 | argv/registry/source decode or IO failure  | original refusal before success |
    # | M29 | regex shortcut/alias/hidden/decoder edges  | original source predicate unchanged |
    # | M30 | current real corpus                        | original member-only assertions above |
    # Constraints: len(reason), not strip, has minimum30; direct-walk regex and
    # runner filename-substring are not replaced by stronger AST predicates.
    # No fixture calls Git/Cargo or writes the real registry/baselines.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        scripts = root / "scripts/ci"
        scripts.mkdir(parents=True)
        primary = scripts / "check_probe.py"
        other = scripts / "check_second.py"
        runner = scripts / "run-gate.sh"
        registry_path = root / "registry.json"
        list_source = 'OWNERS = ("crates/a.rs", "crates/b.rs")\n'
        delegated = ('import formal_sources\nformal_sources.tracked_files(ROOT)\n'
                     'if __name__ == "__main__": pass\n')
        direct = 'list(ROOT.glob("*.rs"))\nif __name__ == "__main__": pass\n'
        reason = "a" * MIN_REASON
        empty = {"scope_lists": {}, "scanners_without_input_globs": {},
                 "baseline": {"scope_lists": 0, "scanners_without_input_globs": 0}}
        registered = {
            "scope_lists": {"check_probe.py:OWNERS": {"reason": reason}},
            "scanners_without_input_globs": {"check_probe.py": {"reason": reason, "runner": None}},
            "baseline": {"scope_lists": 1, "scanners_without_input_globs": 1},
        }
        original_open, original_parse = Path.open, ast.parse

        def invoke(record, argv=None, replacement=None, refusal=None, registry_fault=None,
                   runner_replacement=None):
            if registry_fault == "missing":
                registry_path.unlink(missing_ok=True)
            elif registry_fault == "json":
                registry_path.write_text("{invalid\n", encoding="utf-8")
            elif registry_fault == "utf8":
                registry_path.write_bytes(b"\xff")
            else:
                registry_path.write_text(json.dumps(record), encoding="utf-8")
            before = {path: path.read_bytes() for path in (registry_path, primary, other, runner)
                      if path.is_file()}
            reads, parses = Counter(), []

            def opened(path, *args, **kwargs):
                reads[path] += 1
                if path == refusal:
                    raise PermissionError("fixture reader refused")
                if replacement is not None and path == primary and reads[path] > 1:
                    return io.TextIOWrapper(io.BytesIO(replacement), encoding="utf-8")
                if runner_replacement is not None and path == runner and reads[path] > 1:
                    return io.TextIOWrapper(io.BytesIO(runner_replacement), encoding="utf-8")
                return original_open(path, *args, **kwargs)

            def parsed(source, *args, **kwargs):
                parses.append(source)
                return original_parse(source, *args, **kwargs)

            output, errors = io.StringIO(), io.StringIO()
            failure = None
            with patch.dict(main.__globals__, {"REGISTRY": registry_path}), \
                    patch.object(Path, "open", opened), patch.object(ast, "parse", parsed), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    status = main(["--root", str(root)] if argv is None else argv)
                except SystemExit as error:
                    status = error.code
                except (OSError, UnicodeError, SyntaxError, AttributeError, json.JSONDecodeError) as error:
                    status, failure = 1, type(error)
            assert before == {path: path.read_bytes() for path in before}, "M24-M29 fixture bytes unchanged"
            if registry_fault == "missing":
                assert not registry_path.exists(), "M28 no registry initialization"
            return status, output.getvalue(), errors.getvalue(), reads, parses, failure

        primary.write_text(list_source + delegated, encoding="utf-8")
        other.write_text(direct, encoding="utf-8")
        runner.write_text("check_probe.py\ncheck_second.py\n", encoding="utf-8")
        both = json.loads(json.dumps(registered))
        both["scanners_without_input_globs"] = {
            name: {"reason": reason, "runner": "scripts/ci/run-gate.sh"}
            for name in (primary.name, other.name)
        }
        both["baseline"]["scanners_without_input_globs"] = 2
        status, output, _, reads, parses, _ = invoke(both, runner_replacement=b"true\n")
        assert status == 0 and "1 registered scope lists and 2 registered" in output, "M24 admission/report"
        assert reads == {registry_path: 1, primary: 1, other: 1, runner: 1}, "M24 physical reads"
        assert parses == [list_source + delegated, direct], "M24 original actual AST parses once"
        other.unlink()

        primary.write_text(delegated, encoding="utf-8")
        status, _, errors, reads, parses, _ = invoke(empty, replacement=b"pass\n")
        assert status == 1 and "declares no INPUT_GLOBS" in errors, "M25a first scanner is never hidden"
        assert reads[primary] == 1 and parses == [delegated], "M25a no second source"
        primary.write_text(list_source, encoding="utf-8")
        only_list = json.loads(json.dumps(registered))
        only_list["scanners_without_input_globs"] = {}
        only_list["baseline"]["scanners_without_input_globs"] = 0
        later = (list_source + 'EXTRA = ("crates/c.rs", "crates/d.rs")\n').encode()
        status, output, _, reads, _, _ = invoke(only_list, replacement=later)
        assert status == 0 and "1 registered scope lists and 0 registered" in output, "M25b first report"
        assert reads[primary] == 1, "M25b success does not reacquire"

        primary.write_text(list_source + delegated, encoding="utf-8")
        # M31-M33: original scalar facts -> the existing canonical comparison
        # at each original error phase. Missing/None is an independent hard
        # precondition, never the comparator's implicit zero allowance.
        #
        # | Row | held/count                          | effect |
        # | M31 | 0/1, 1/1, 2/1, 1.5/1              | grow, equal, fall, strict fractional fall |
        # | M32 | missing/None × other field valid   | hard missing, compare only valid field |
        # | M33 | NoWriter growth/fall               | real rule:key old->new and registry/source fix |
        # The original M26 hard-rule matrix remains below; no diagnostic grants
        # a writer, raises a budget, consumes a stale error or rereads a source.
        for value in (0, 1, 2, 1.5):
            record = json.loads(json.dumps(registered))
            record["baseline"] = {"scope_lists": value, "scanners_without_input_globs": value}
            status, _, errors, _, _, _ = invoke(record)
            assert (status == 0) is (value == 1), "M31 original strict scalar relation"
            if value != 1:
                assert f"W1: scope_lists {value} -> 1; fix: " in errors, "M33 W1 diagnostic"
                assert f"W2: scanners_without_input_globs {value} -> 1; fix: " in errors, "M33 W2 diagnostic"
                assert errors.index("W1:") < errors.index("W2:"), "M33 original phase order"
                assert "--tighten" not in errors and "--write-baseline" not in errors, "M33 truthful NoWriter"
                if value > 1:
                    assert "baseline.scope_lists" in errors and "baseline.scanners_without_input_globs" in errors, \
                        "M33 real lower registry coordinates"
        original_compare = _architecture_ratchet.compare
        calls = []

        def compared(*args, **kwargs):
            calls.append(args[0][0].name)
            return original_compare(*args, **kwargs)

        with patch.object(_architecture_ratchet, "compare", compared):
            assert invoke(registered)[0] == 0 and calls == ["W1", "W2"], "M31 both actual callbacks canonical"
            for field, other_rule in (("scope_lists", "W2"), ("scanners_without_input_globs", "W1")):
                for missing in (True, False):
                    record = json.loads(json.dumps(registered))
                    if missing:
                        record["baseline"].pop(field)
                    else:
                        record["baseline"][field] = None
                    calls.clear()
                    status, _, errors, _, _, _ = invoke(record)
                    assert status == 1 and f"must carry a baseline count for {field}" in errors, "M32 missing hard"
                    assert calls == [other_rule], "M32 absent allowance not passed as zero"
        # M26 crosses both independent count contracts with short/valid/space
        # reasons and stale members. Missing count records are hard too; an
        # equal or lower numeric value cannot consume either metadata error.
        for first, second, text, stale in product((None, 0, 1, 2), (None, 0, 1, 2),
                                                  ("a" * 29, reason, " " * 30), (False, True)):
            record = json.loads(json.dumps(registered))
            for key, value in (("scope_lists", first), ("scanners_without_input_globs", second)):
                if value is None:
                    record["baseline"].pop(key)
                else:
                    record["baseline"][key] = value
            record["scope_lists"]["check_probe.py:OWNERS"]["reason"] = text
            record["scanners_without_input_globs"][primary.name]["reason"] = text
            if stale:
                record["scope_lists"]["check_probe.py:GONE"] = {"reason": reason}
                record["scanners_without_input_globs"]["check_gone.py"] = {"reason": reason, "runner": None}
            status, _, errors, _, _, _ = invoke(record)
            expected = first == second == 1 and len(text) >= MIN_REASON and not stale
            assert (status == 0) is expected, ("M26", first, second, len(text), stale, errors)
            if len(text) < MIN_REASON:
                assert errors.count("must say why") == 2, "M26 both short reasons remain hard"
            if stale:
                assert "no longer exists" in errors and "now declares" in errors, "M26 both stale decisions hard"
        for name, content, expected in ((None, "", 0), ("run-gate.sh", "check_probe.py", 0),
                                         ("run-gate.sh", "# check_probe.py is still the original substring", 0),
                                         ("run-gate.sh", "true", 1), ("missing.sh", "", 1)):
            record = json.loads(json.dumps(registered))
            record["scanners_without_input_globs"][primary.name]["runner"] = (
                None if name is None else "scripts/ci/" + name)
            runner.write_text(content, encoding="utf-8")
            status, _, _, reads, _, _ = invoke(record)
            assert status == expected, "M27 original runner is_file/substring/None"
            assert reads[runner] == (1 if name == runner.name else 0), "M27 conditional runner reads"
        record = json.loads(json.dumps(registered))
        record["scanners_without_input_globs"][primary.name]["runner"] = "scripts/ci/run-gate.sh"
        assert invoke(record, refusal=runner)[-1] is PermissionError, "M27 unreadable runner propagates"

        # M28 deliberately refuses the real registry reader for invalid argv;
        # every source failure is caught only by this test harness, never by a
        # production empty-corpus fallback or a new exception policy.
        status, _, errors, reads, _, _ = invoke(empty, argv=["--unknown"], refusal=registry_path)
        assert status == 2 and "usage:" in errors and not reads, "M28 argv before IO"
        assert invoke(empty, refusal=registry_path)[-1] is PermissionError, "M28 registry failure"
        for fault, expected in (("missing", FileNotFoundError), ("json", json.JSONDecodeError),
                                ("utf8", UnicodeDecodeError)):
            _, _, _, reads, parses, failure = invoke(empty, registry_fault=fault)
            assert failure is expected and reads == {registry_path: 1} and not parses, \
                "M28 original registry failure precedes module acquisition"
        status, _, _, reads, _, failure = invoke(None)
        assert failure is AttributeError and reads == {registry_path: 1}, "M28 registry type before module IO"
        assert invoke(registered, refusal=primary)[-1] is PermissionError, "M28 source IO failure"
        primary.write_bytes(b"# \xff\n")
        assert invoke(empty)[-1] is UnicodeDecodeError, "M28 original strict UTF8"
        primary.write_text("!invalid\n", encoding="utf-8")
        assert invoke(empty)[-1] is SyntaxError, "M28 original syntax failure"
        primary.unlink()
        primary.mkdir()
        assert invoke(empty)[-1] is IsADirectoryError, "M28 no new is_file corpus filter"
        primary.rmdir()
        for newline in ("\r\n", "\r"):
            primary.write_bytes((list_source + delegated).replace("\n", newline).encode())
            status, _, _, reads, parses, _ = invoke(registered)
            assert status == 0 and reads[primary] == 1 and parses == [list_source + delegated], \
                "M28 universal-newline projection unchanged"

        # M29 preserves the direct regex's prose shortcut and every existing
        # delegated alias decision; a helper's hidden/sourced name is no filter.
        for name in (".hidden.py", "_sourced.py"):
            hidden = scripts / name
            hidden.write_text(list_source, encoding="utf-8")
            modules = _module_sources(scripts)
            assert hidden in modules and scope_lists(modules[hidden][1]), "M29 W1 includes every module name"
            hidden.unlink()
        prose = '# ROOT.glob("*.rs")\nif __name__ == "__main__": pass\n'
        assert undeclared_scanners({primary: (prose, ast.parse(prose))}) == [primary.name], \
            "M29 direct regex shortcut remains unchanged"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        selftest()
        return 0

    registry = load_registry(REGISTRY)
    errors, live, undeclared = validation_errors(arguments.root, registry)
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
    print(
        f"OK - {len(live)} registered scope lists and "
        f"{len(undeclared)} registered "
        "undeclared scanners, none new."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
