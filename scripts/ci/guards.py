#!/usr/bin/env python3
"""Find a guard and read its own rule documentation without executing it.

`list` shows Python check entry points and the architecture fitness modules.
`explain <rule>` accepts a guard's file stem or a rule declared in its RULES
or documented definition. A repeated rule needs `<guard>:<rule>`. Names,
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
# A definition names a rule and ends its name (and optional qualification)
# with a colon. Merely mentioning a checker in a historical bullet is not a
# rule declaration. Numbered rule IDs use the same explicit colon spelling.
DOCUMENTED_RULE = re.compile(r"^\* `([a-z][a-z0-9_-]*)`(?:\s+\([^\n)]*\))?:", re.MULTILINE)
NUMBERED_RULE = re.compile(r"^([A-Z][0-9]+):\s", re.MULTILINE)


@dataclass(frozen=True)
class Guard:
    path: str
    documentation: str
    rules: tuple[str, ...]

    @property
    def name(self) -> str:
        return Path(self.path).stem


def rule_names(tree: ast.Module, documentation: str) -> tuple[str, ...]:
    """Read literal rule declarations and explicit documentation definitions."""
    names = set(DOCUMENTED_RULE.findall(documentation)) | set(NUMBERED_RULE.findall(documentation))
    for statement in tree.body:
        if not isinstance(statement, (ast.Assign, ast.AnnAssign)):
            continue
        targets = statement.targets if isinstance(statement, ast.Assign) else [statement.target]
        if not any(isinstance(target, ast.Name) and target.id == "RULES" for target in targets):
            continue
        value = statement.value
        if isinstance(value, ast.Dict):
            # An ID -> literal advice dictionary declares rules. A source path
            # -> pattern tuple dictionary belongs to its scanner, not navigation.
            elements = [key for key, advice in zip(value.keys, value.values)
                        if all(isinstance(part, ast.Constant) and isinstance(part.value, str)
                               for part in (key, advice))]
        else:
            elements = value.elts if isinstance(value, (ast.Tuple, ast.List)) else []
        for item in elements:
            if isinstance(item, ast.Call) and isinstance(item.func, ast.Name) and item.func.id == "Rule":
                item = item.args[0] if item.args else None
            elif (isinstance(item, (ast.Tuple, ast.List)) and len(item.elts) == 2
                  and all(isinstance(part, ast.Constant) and isinstance(part.value, str) for part in item.elts)):
                # Some owners keep (rule ID, label) pairs. The label is advice,
                # not a second rule, and no expression is evaluated here.
                item = item.elts[0]
            if isinstance(item, ast.Constant) and isinstance(item.value, str):
                names.add(item.value)
    return tuple(sorted(names))


def guards(root: Path) -> tuple[Guard, ...]:
    """Discover source owners; never import a guard or run its measurement."""
    found = []
    for path in sorted((root / "scripts/ci").glob("*.py")):
        fitness = path.name.endswith("_fitness.py")
        if not (fitness or path.name.startswith("check_")):
            continue
        source = path.read_bytes()
        if not fitness and not is_entry_point(path, source.decode("utf-8", errors="ignore")):
            continue
        tree = ast.parse(source.decode("utf-8"), filename=str(path))
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
    N3 string/Rule/dict/tuple/definition rule -> lookup; historical mentions and
    tuple labels are excluded; N4 ambiguity/unknown -> refusal;
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
        first.write_text('"""First owner.\n\n* `documented`: original advice.\n'
                         '* `qualified` (a hard rule): actual qualification.\n'
                         'W1: explicit numbered rule.\n'
                         '* `historical_checker` scanned a different corpus in the past.\n"""\n'
                         'raise RuntimeError("must not import")\n'
                         'RULES = ("string-rule", Rule("shared", "fix"), ("tuple-rule", "tuple label"),\n'
                         '         ["list-rule", "list label"], ("dynamic-rule", dangerous()), (42, "not an ID"))\n'
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
        for key in ("check_first", "string-rule", "documented", "qualified", "W1", "shared", "tuple-rule", "list-rule"):
            status, output, _ = invoke(["explain", key])
            assert status == 0 and "First owner" in output and "original advice" in output, "N1/N3/N6"
        # N3 causes: literal ID/label tuples and explicit definition colon;
        # historical mentions, labels, dynamic/non-string tuples. Effects:
        # exact IDs resolve, all other selectors refuse; source bytes unchanged
        # and neither the guard nor its dangerous() expression is executed.
        for key in ("historical_checker", "tuple label", "list label", "dynamic-rule", "not an ID"):
            status, _, errors = invoke(["explain", key])
            assert status == 2 and "unknown" in errors, "N3 only authoritative literal rule definitions"
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
    _selftest_read_boundary()


def _selftest_read_boundary() -> None:
    """Keep acquisition, entry selection and rule documentation on one source."""
    import contextlib
    import io
    import tempfile
    from collections import Counter
    from unittest.mock import patch

    # N9-N12 cause/effect graph: valid argv -> one raw acquisition -> original
    # ignore-decoded entry predicate -> strict-decoded selected AST -> docs/IDs.
    # Invalid argv stops before acquisition; excluded candidates never reach
    # strict decoding. Only literal ID -> literal advice dict rows declare IDs.
    #
    # | Row | cause                                  | effect |
    # | N9  | second read would replace docs/IDs      | one read/parse; first docs/IDs |
    # | N10 | string advice / pattern or dynamic data | declare / ignore, never execute |
    # | N11 | malformed RULES / unknown selector     | no invented ID; original refusal |
    # | N12 | invalid bytes non-entry / entry        | skip / original strict decode failure |
    # | N12 | invalid argv / IO failure / bad syntax | no read / error1 / error1 |
    # Constraints: source bytes stay unchanged; a fitness module needs no main
    # guard; __main__ substring selection and both decoder policies stay exact.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        directory = root / "scripts/ci"
        directory.mkdir(parents=True)
        first = directory / "check_first.py"
        initial = (b'"""First snapshot."""\r\nRULES = {"first": "advice"}\r\n'
                   b'if __name__ == "__main__": raise RuntimeError("never run")\r\n')
        replacement = b'"""Second snapshot."""\nRULES = {"second": "different"}\n'
        first.write_bytes(initial)
        fitness = directory / "_fixture_fitness.py"
        fitness.write_text('"""Fitness without a main guard."""\nRULES = ("fitness",)\n')
        excluded = directory / "check_nonentry.py"
        excluded.write_bytes(b"# not an entry point\n\xff\n")
        before = {path: path.read_bytes() for path in directory.iterdir()}
        reads, parses = Counter(), Counter()
        original_open, original_parse = Path.open, ast.parse

        def opened(path, *args, **kwargs):
            if path.parent != directory:
                return original_open(path, *args, **kwargs)
            reads[path.name] += 1
            if path == first and reads[path.name] > 1:
                raw = io.BytesIO(replacement)
                return raw if "b" in (args[0] if args else kwargs.get("mode", "r")) else io.TextIOWrapper(
                    raw, encoding=kwargs.get("encoding", "utf-8"), errors=kwargs.get("errors", "strict"))
            return original_open(path, *args, **kwargs)

        def parsed(source, *args, **kwargs):
            filename = kwargs.get("filename", args[0] if args else "<unknown>")
            parses[Path(filename).name] += 1
            return original_parse(source, *args, **kwargs)

        with patch.object(Path, "open", opened), patch.object(ast, "parse", parsed):
            found = guards(root)
        assert reads == {first.name: 1, fitness.name: 1, excluded.name: 1}, "N9 one physical read"
        assert parses == {first.name: 1, fitness.name: 1}, "N9 one selected AST, non-entry excluded"
        assert found == (Guard("scripts/ci/_fixture_fitness.py", "Fitness without a main guard.", ("fitness",)),
                         Guard("scripts/ci/check_first.py", "First snapshot.", ("first",))), "N9 same snapshot"
        assert before == {path: path.read_bytes() for path in directory.iterdir()}, "N9 unchanged bytes"

        def invoke(argv):
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    status = main(argv, root=root)
                except SystemExit as error:
                    status = error.code
            return status, output.getvalue(), errors.getvalue()

        # N10 uses a path-looking real ID too: only the declaration's shape
        # decides, never a path vocabulary or a second rule catalog.
        first.write_text('"""Dictionary owner."""\nRULES = {"real": "advice", '
                         '"crates/real.rs": "literal advice", "crates/pattern.rs": ("token",), '
                         '"list-data": ["token"], "dynamic": dangerous(), dangerous(): "value", '
                         '42: "not an ID", **dangerous()}\n'
                         'if __name__ == "__main__": raise RuntimeError("never run")\n')
        assert guards(root)[1].rules == ("crates/real.rs", "real"), "N10 literal advice only"
        for selector in ("real", "crates/real.rs", "check_first:real"):
            status, output, _ = invoke(["explain", selector])
            assert status == 0 and "Dictionary owner." in output, "N10 true declarations resolve"
        for selector in ("crates/pattern.rs", "check_first:crates/pattern.rs", "list-data", "dynamic", "not an ID"):
            status, _, errors = invoke(["explain", selector])
            assert status == 2 and "unknown rule" in errors, "N10 data is not a declared rule"
        for declaration in ("42", "dangerous()", '{"missing": None}', '{**dangerous()}'):
            first.write_text(f'"""Malformed declaration."""\nRULES = {declaration}\n'
                             'if __name__ == "__main__": pass\n')
            assert not guards(root)[1].rules, "N11 malformed declarations invent no IDs"
            assert invoke(["explain", "missing"])[0] == 2, "N11 original unknown refusal"

        first.write_bytes(initial + b"# \xff\n")
        try:
            guards(root)
        except UnicodeDecodeError:
            pass
        else:
            raise AssertionError("N12 selected entry retains strict UTF8 failure")
        first.write_bytes(initial)
        with patch.object(Path, "open", side_effect=AssertionError("must parse argv before reads")):
            assert invoke(["list", "--write-baseline"])[0] == 2, "N12 invalid argv makes no acquisition"
        with patch.object(Path, "open", side_effect=PermissionError("unreadable source")):
            status, _, errors = invoke(["list"])
            assert status == 1 and "cannot read guard documentation" in errors, "N12 original IO refusal"
        first.write_text('if __name__ == "__main__":\n!invalid\n')
        status, _, errors = invoke(["list"])
        assert status == 1 and "cannot read guard documentation" in errors, "N12 original syntax refusal"
    print("guards single-source self-test: N9-N12 hold")


if __name__ == "__main__":
    raise SystemExit(main())
