#!/usr/bin/env python3
"""Find a guard and read its own rule documentation without executing it.

`list` shows Python check entry points and the architecture fitness modules.
`explain <rule>` accepts a guard's file stem or a rule declared in its RULES
or documented bullet list. A repeated rule needs `<guard>:<rule>`. Names,
descriptions and advice come from those source files; there is no second
catalog, baseline writer or check dispatcher here.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path

from check_gate_arguments import is_entry_point

INPUT_GLOBS: tuple[str, ...] = ("scripts/ci/*.py",)
ROOT = Path(__file__).resolve().parents[2]
DOCUMENTED_RULE = re.compile(r"^\* `([a-z][a-z0-9_-]*)`(?=[:\s(])", re.MULTILINE)


@dataclass(frozen=True)
class Guard:
    path: str
    documentation: str
    rules: tuple[str, ...]

    @property
    def name(self) -> str:
        return Path(self.path).stem


def rule_names(tree: ast.Module, documentation: str) -> tuple[str, ...]:
    """Read declarations as syntax, including Rule(name, advice) and strings."""
    names = set(DOCUMENTED_RULE.findall(documentation))
    for statement in tree.body:
        if not isinstance(statement, (ast.Assign, ast.AnnAssign)):
            continue
        targets = statement.targets if isinstance(statement, ast.Assign) else [statement.target]
        if not any(isinstance(target, ast.Name) and target.id == "RULES" for target in targets):
            continue
        value = statement.value
        elements = value.elts if isinstance(value, (ast.Tuple, ast.List)) else (
            value.keys if isinstance(value, ast.Dict) else []
        )
        for item in elements:
            if isinstance(item, ast.Call) and isinstance(item.func, ast.Name) and item.func.id == "Rule":
                item = item.args[0] if item.args else None
            if isinstance(item, ast.Constant) and isinstance(item.value, str):
                names.add(item.value)
    return tuple(sorted(names))


def guards(root: Path) -> tuple[Guard, ...]:
    """Discover source owners; never import a guard or run its measurement."""
    found = []
    for path in sorted((root / "scripts/ci").glob("*.py")):
        if not (path.name.endswith("_fitness.py") or
                (path.name.startswith("check_") and is_entry_point(path))):
            continue
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        documentation = ast.get_docstring(tree) or ""
        found.append(Guard(path.relative_to(root).as_posix(), documentation,
                           rule_names(tree, documentation)))
    return tuple(found)


def select(available: tuple[Guard, ...], selector: str) -> tuple[Guard, ...]:
    if ":" in selector:
        owner, rule = selector.split(":", 1)
        return tuple(guard for guard in available if guard.name == owner and rule in guard.rules)
    direct = tuple(guard for guard in available if selector in (guard.name, guard.path))
    return direct or tuple(guard for guard in available if selector in guard.rules)


def main(argv: list[str] | None = None, *, root: Path = ROOT) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true", help="run the navigation decision table")
    commands = parser.add_subparsers(dest="command")
    commands.add_parser("list", allow_abbrev=False, help="list guards and their declared rules")
    explain = commands.add_parser("explain", allow_abbrev=False, help="read the owning guard's documentation")
    explain.add_argument("rule", help="guard stem, rule id, or guard:rule for an ambiguous id")
    args = parser.parse_args(argv)
    if args.self_test:
        if args.command:
            parser.error("--self-test cannot be combined with a command")
        self_test()
        return 0
    if args.command is None:
        parser.error("choose list or explain")
    try:
        available = guards(root)
    except (OSError, SyntaxError) as error:
        print(f"ERROR: cannot read guard documentation: {error}", file=sys.stderr)
        return 1
    if args.command == "list":
        for guard in available:
            summary = guard.documentation.splitlines()[0] if guard.documentation else "(no module documentation)"
            print(f"{guard.name}: {summary}")
            if guard.rules:
                print(f"  rules: {', '.join(guard.rules)}")
        return 0
    matches = select(available, args.rule)
    if len(matches) != 1:
        detail = ("unknown rule" if not matches else
                  "ambiguous rule; choose " + ", ".join(f"{guard.name}:{args.rule}" for guard in matches))
        print(f"ERROR: {args.rule}: {detail}; run python3 scripts/ci/guards.py list", file=sys.stderr)
        return 2
    guard = matches[0]
    if not guard.documentation:
        print(f"ERROR: {guard.path}: no module documentation; read the source", file=sys.stderr)
        return 1
    print(f"Source: {guard.path}\n\n{guard.documentation}")
    return 0


def self_test() -> None:
    """Navigation cause/effect table.

    Causes: existing/new/removed source owner, declared/documented rule,
    exact/unknown/ambiguous selector, valid/invalid arguments, syntax failure.
    Effects: docs from the exact owner, deterministic discovery, refusal, and
    unchanged source bytes. Constraints: parsing precedes reads; a guard's
    executable body and measurement never run; no persisted navigation catalog.
    N1 list/explain -> source docs; N2 add/remove -> immediate discovery;
    N3 string/Rule/dict/doc rule -> lookup; N4 ambiguity/unknown -> refusal;
    N5 invalid argv -> usage before reads; N6 executable body -> never run;
    N7 bad source -> fail closed; N8 missing docs -> list the owner, explain
    refuses to invent advice. Every row is read-only.
    """
    import contextlib
    import io
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        directory = root / "scripts/ci"
        directory.mkdir(parents=True)
        first = directory / "check_first.py"
        first.write_text('"""First owner.\n\n* `documented`: original advice.\n"""\n'
                         'raise RuntimeError("must not import")\n'
                         'RULES = ("string-rule", Rule("shared", "fix"))\n'
                         'if __name__ == "__main__":\n    raise RuntimeError("must not execute")\n')
        (directory / "helper.py").write_text('"""Not a guard."""\n')
        (directory / "baseline.json").write_text('{"held": 7}\n')

        def invoke(argv):
            before = {path: path.read_bytes() for path in directory.iterdir()}
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    status = main(argv, root=root)
                except SystemExit as error:
                    status = error.code
            assert before == {path: path.read_bytes() for path in directory.iterdir()}, "N1-N8 read-only"
            return status, output.getvalue(), errors.getvalue()

        status, output, _ = invoke(["list"])
        assert status == 0 and "check_first" in output and "helper" not in output, "N1/N6"
        for key in ("check_first", "string-rule", "documented", "shared"):
            status, output, _ = invoke(["explain", key])
            assert status == 0 and "First owner" in output and "original advice" in output, "N1/N3/N6"
        second = directory / "_second_fitness.py"
        second.write_text('"""Second owner."""\nRULES = {"shared": "advice"}\n')
        status, output, _ = invoke(["list"])
        assert status == 0 and "_second_fitness" in output, "N2 added"
        status, _, errors = invoke(["explain", "shared"])
        assert status == 2 and "ambiguous" in errors and "_second_fitness:shared" in errors, "N4"
        status, output, _ = invoke(["explain", "_second_fitness:shared"])
        assert status == 0 and "Second owner" in output, "N3/N4 qualified"
        second.unlink()
        status, _, _ = invoke(["explain", "shared"])
        assert status == 0, "N2 removed, no stale catalog"
        second.write_text('RULES = ("undocumented",)\n')
        status, output, _ = invoke(["list"])
        assert status == 0 and "_second_fitness: (no module documentation)" in output, "N8 list"
        status, _, errors = invoke(["explain", "undocumented"])
        assert status == 1 and "no module documentation" in errors, "N8 explain"
        second.unlink()
        for key in ("missing", "check_first:missing", "../../outside"):
            status, _, errors = invoke(["explain", key])
            assert status == 2 and "unknown" in errors, "N4"
        # Invalid argv must refuse before even parsing this broken source.
        first.write_text('"""Broken guard."""\n!invalid\nif __name__ == "__main__": pass\n')
        for argv in ([], ["unknown"], ["--self-tes"], ["--self-test", "list"],
                     ["list", "--write-baseline"], ["explain"]):
            status, _, errors = invoke(argv)
            assert status == 2 and "usage:" in errors and "cannot read" not in errors, "N5"
        status, _, errors = invoke(["list"])
        assert status == 1 and "cannot read guard documentation" in errors, "N7"
    print("guards self-test: the decision table holds")


if __name__ == "__main__":
    raise SystemExit(main())
