#!/usr/bin/env python3
"""Hold the Python CI layer at its ceiling: scripts/ci may not grow.

The layer was meant to shrink to about 72,000 lines, but no further rule could
be deleted without losing a check no other tool holds, so the decision (user,
2026-11-08) is "no growth": the count measured when this guard landed is the
ceiling, and it only moves down.

* a count above the ceiling fails, naming how many lines over it the change is:
  shrink scripts/ci (delete a rule a type, Cargo or clippy now holds, merge a
  duplicate) in the same commit;
* a count below the ceiling passes and suggests ``--tighten``, which lowers the
  recorded ceiling to the measured count;
* raising the ceiling is the reviewed edit ``--write-baseline --reason '<JSON
  review>'``; the reason stays next to the number in growth_reasons.

The count is principle_status.py's V1 "Python lines", one definition for both.
The shared ratchet CLI (_architecture_ratchet.baseline_cli) owns the modes and
the baseline IO.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import _architecture_ratchet
from principle_status import python_lines

INPUT_GLOBS: tuple[str, ...] = ("scripts/ci/*.py", "scripts/ci/python_lines_baseline.json")

SCRIPTS = Path(__file__).resolve().parent
BASELINE = SCRIPTS / "python_lines_baseline.json"
RULE = _architecture_ratchet.Rule("python lines", "")
KEY = "scripts/ci"
PROG = "check_python_ceiling.py"
COMMENT = (f"Ceiling of the Python lines in scripts/ci/*.py, held by scripts/ci/{PROG}. Lower it with --tighten; "
           "raising it needs --write-baseline --reason, kept in growth_reasons.")


def ceiling(baseline: dict) -> int:
    return baseline.get(RULE.name, {}).get(KEY, 0)


def lines_text(count: int) -> str:
    return f"{count} line{'s' * (count != 1)}"


def check(lines: int, baseline: dict) -> list[str]:
    over = lines - ceiling(baseline)
    if over <= 0:
        return []
    return [_architecture_ratchet.diagnostic(
        "Python CI ceiling", KEY, ceiling(baseline), lines,
        f"shrink scripts/ci by {lines_text(over)} in this commit (the layer holds no growth); a reviewed raise is "
        f"python3 scripts/ci/{PROG} --write-baseline --reason '<JSON review>'",
        details=f"{lines_text(over)} over the ceiling in {BASELINE.name}")]


def status(lines: int, baseline: dict) -> str:
    under = ceiling(baseline) - lines
    if not under:
        return f"OK - scripts/ci holds {lines} Python lines, at its ceiling."
    return (f"OK - scripts/ci holds {lines} Python lines, {under} below its ceiling {ceiling(baseline)}; "
            f"lower the ceiling with python3 scripts/ci/{PROG} --tighten")


def record(lines: int, previous: dict, reason: str | None = None) -> dict:
    return _architecture_ratchet.render((RULE,), {RULE.name: {KEY: lines}}, previous, reason,
                                        previous.get("$comment", COMMENT))


def main(argv: list[str] | None = None) -> int:
    return _architecture_ratchet.baseline_cli(
        argv, prog=PROG, description=__doc__.splitlines()[0], baseline_path=BASELINE, self_test=self_test,
        measure=lambda: ([], python_lines(SCRIPTS)), check=check, check_message=status,
        tighten=lambda lines, previous: record(min(lines, ceiling(previous)), previous),
        write=lambda lines, previous, reason: (
            record(lines, previous, reason), _architecture_ratchet.growth((RULE,), {RULE.name: {KEY: lines}}, previous)))


def self_test() -> None:
    """| Row | measured vs ceiling 100 | mode | effect |
    | C1 | 100 | check | pass, no suggestion |
    | C2 | 101 | check / tighten | fail: "1 line over", baseline bytes unchanged |
    | C3 | 90 | check | pass, suggests --tighten |
    | C4 | 90 | tighten | ceiling lowered to 90 |
    | C5 | 101 | write without / with a review | refused / raised, reason recorded |
    | C6 | a directory | measure | every line of its top-level *.py, nothing else |
    """
    import tempfile

    held = {"$comment": "fixture", RULE.name: {KEY: 100}}

    def invoke(argv, lines):
        return _architecture_ratchet._cli_fixture(main, argv, held, "python_lines", lines)

    status_, before, after, _, out = invoke([], 100)
    assert status_ == 0 and before == after and "at its ceiling" in out, "C1"
    for argv in ([], ["--tighten"]):
        status_, before, after, _, _ = invoke(argv, 101)
        assert status_ == 1 and before == after, f"C2 {argv}"
    errors = check(101, held)
    assert len(errors) == 1 and "1 line over the ceiling" in errors[0] and "shrink scripts/ci by 1 line in" in errors[0], "C2"
    status_, before, after, _, out = invoke([], 90)
    assert status_ == 0 and before == after and "10 below its ceiling 100" in out and "--tighten" in out, "C3"
    status_, _, after, _, _ = invoke(["--tighten"], 90)
    assert status_ == 0 and ceiling(json.loads(after)) == 90, "C4"
    status_, before, after, _, _ = invoke(["--write-baseline"], 101)
    assert status_ == 1 and before == after, "C5 a raise needs a review"
    review = _architecture_ratchet._test_growth_reason("reviewed raise")
    status_, _, after, _, _ = invoke(["--write-baseline", "--reason", review], 101)
    raised = json.loads(after)
    assert status_ == 0 and ceiling(raised) == 101 and raised["growth_reasons"][RULE.name][KEY] == review, "C5"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "a.py").write_text("1\n2\n", encoding="utf-8")
        (root / "b.py").write_text("3", encoding="utf-8")
        (root / "notes.md").write_text("x\n", encoding="utf-8")
        (root / "sub").mkdir()
        (root / "sub/c.py").write_text("x\n", encoding="utf-8")
        assert python_lines(root) == 3, "C6"
    print("check_python_ceiling self-test: C1-C6 passed")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
