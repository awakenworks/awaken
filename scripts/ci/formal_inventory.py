#!/usr/bin/env python3
"""Classify every Kani harness by what its recorded runs say, and plan the reruns.

This report derives resource-bounded proof batches from the existing plan
(``formal_plan``), the audited profiles (``formal_kani_options``) and the Kani
transcripts given to it, which harnesses are known to finish inside the gate's
budget and which are not, and it packs the plan's batches into build slots::

    formal_inventory.py [TRANSCRIPT ... | @LIST] [--table] [--slots | --slot N] [--first TEXT ...]
                        [--then TEXT ...] [--obligations]

Classes (one per harness; the first rule that matches wins):

* ``b`` the last recorded run did not finish (timeout, out of memory, killed),
  failed, or passed with an unsatisfied ``kani::cover!`` (vacuous, see
  ``formal_vacuity``), or its slowest finished run used half the batch deadline;
* ``d`` never recorded, or the harness body or its package-bound loop profile
  was changed (author time, ``git blame``) after its last recorded run;
* ``a`` otherwise: the last run passed on the current harness text.

A transcript is any log holding Kani's ``Checking harness <name>...`` lines
(``--output-format terse`` or regular). Its modification time is the run time.
A run is matched by function name, which is unique across the plan, so a run
recorded before a module or crate move still counts (``git blame`` then says
whether the move edited the body).
Merging duplicate obligations (docs/design/formal-proof-trust.md)
is a review judgement; the report only groups harnesses that share a package,
an obligation set and their production calls, as candidates.

Obligations (``--obligations``). A ``kernel_proved`` obligation is backed
only when every harness that serves it is in class ``a``. The ledger marks
the others ``run_evidence = "needs_rerun"`` (``formal_ledger``); the report
lists each kernel-proved obligation that is not backed or whose marker
disagrees with the runs, so a marker is added or cleared from evidence.

Consumers select batches under explicit resource budgets. Generated tables remain reports, not a second maintained proof inventory.
"""

from __future__ import annotations

import argparse
import ast
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

import formal_ledger
import formal_plan
from formal_anchors import BLOCK_LINE
from formal_vacuity import cover_shortfall
from _crate_boundary_workspace import _balanced_end, rust_syntax
from formal_budget import KANI_BATCH_TIMEOUT_CEILING_SECONDS
from formal_kani_options import PROFILES

ROOT = Path(__file__).resolve().parents[2]
OPTIONS = "scripts/ci/formal_kani_options.py"
# A harness that alone uses half the batch deadline leaves no room for the
# seven others its batch may hold; it is at risk, not known to finish.
AT_RISK_SECONDS = KANI_BATCH_TIMEOUT_CEILING_SECONDS // 2
# Measured `cargo kani` start-up per batch (compile, goto-cc, instrument):
# median 66s over the recorded single-batch runs; 90s keeps a margin.
BATCH_OVERHEAD_SECONDS = 90
# One hourly slot keeps ten minutes for queueing behind the admission wrapper.
SLOT_SECONDS = 3000
# Seconds assumed for a harness with no finished run, by static cost class.
UNMEASURED = {"S": 30, "M": 120, "L": 600}

CHECK = re.compile(r"Checking harness ([A-Za-z0-9_:]+)\.\.\.")
RESULT = re.compile(r"VERIFICATION:- (SUCCESSFUL|FAILED)")
TIME = re.compile(r"Verification Time: ([0-9.]+)s")
OOM = re.compile(r"out of memory")
SYMBOLIC = re.compile(r"\bkani::any|\bany(?:_where)?\s*(?:::<[^>]*>)?\s*\(|\b(?:any|arbitrary|symbolic)_\w*\s*\(")
UNWIND = re.compile(r"#\[kani::unwind\((\d+)\)\]")
HEAP = re.compile(r"\b(String|Vec|vec!|BTreeMap|BTreeSet|HashMap|HashSet|serde_json|format!)\b|\.into\(\)")
CALL = re.compile(r"\b([a-z_][a-z0-9_]*)\s*(?:::<[^>]*>)?\s*\(")
GENERIC_CALLS = frozenset(
    "assert assert_eq assert_ne any assume matches some ok err unwrap expect into clone new from iter map "
    "collect len is_some is_none is_ok is_err contains insert get push vec format to_string to_owned as_str "
    "eq ne cmp min max then_some unwrap_or panic unreachable".split()
)


@dataclass(frozen=True)
class Run:
    result: str  # passed | failed | vacuous | unfinished | oom
    seconds: float | None
    when: float
    source: str


@dataclass
class Row:
    planned: formal_plan.Planned
    batch: int
    body_time: int = 0
    profile_time: int = 0
    symbolic: int = 0
    unwind: int | None = None
    heap: bool = False
    calls: tuple[str, ...] = ()
    runs: list[Run] = field(default_factory=list)

    @property
    def name(self) -> str:
        return self.planned.qualified[0]

    @property
    def changed(self) -> int:
        """Author time of the newest edit to the harness body or its bound loop profile."""
        return max(self.body_time, self.profile_time)

    @property
    def last(self) -> Run | None:
        return max(self.runs, key=lambda run: run.when) if self.runs else None

    def current(self) -> list[Run]:
        """Runs of the current harness text: recorded after its newest edit."""
        return [run for run in self.runs if run.when > self.changed]

    def passes(self) -> list[float]:
        """Pass times of the current text, else of the newest text that passed."""
        runs = [run for run in (self.current() or self.runs) if run.result == "passed" and run.seconds]
        if not self.current() and runs:
            newest = max(run.when for run in runs)
            runs = [run for run in runs if run.when == newest]
        return sorted(run.seconds for run in runs)  # type: ignore[misc]

    def slowest(self) -> float | None:
        return self.passes()[-1] if self.passes() else None

    def unfinished(self) -> bool:
        last = (max(self.current(), key=lambda run: run.when) if self.current() else self.last)
        return last is not None and last.result != "passed"

    def static_cost(self) -> str:
        loops = bool(PROFILES[self.planned.selection.profile].loops) if self.planned.selection.profile else False
        if loops or self.planned.tier == "nightly" or (self.unwind or 0) >= 16:
            return "L"
        return "M" if self.heap and self.symbolic >= 4 else "S"

    def cost(self) -> str:
        if self.unfinished():
            return "XL"
        slowest = self.slowest()
        if slowest is None:
            return "~" + self.static_cost()
        return "S" if slowest < 60 else "M" if slowest < 300 else "L" if slowest < AT_RISK_SECONDS else "XL"

    def estimate(self, worst: bool = False) -> float:
        """Seconds to plan for: the median pass (``worst``: the slowest), plus a quarter."""
        if self.unfinished():
            return KANI_BATCH_TIMEOUT_CEILING_SECONDS
        passes = self.passes()
        if not passes:
            return UNMEASURED[self.static_cost()]
        return 1.25 * (passes[-1] if worst else passes[len(passes) // 2])

    def classify(self) -> tuple[str, str]:
        """b: did not finish or is slow; d: no run of the current text; a: it passed."""
        if self.unfinished():
            last = max(self.current() or self.runs, key=lambda run: run.when)
            return "b", f"last run {last.result}" + ("" if self.current() else ", text changed since")
        slowest = self.slowest()
        if slowest is not None and slowest >= AT_RISK_SECONDS:
            return "b", f"slowest pass {slowest:.0f}s"
        if not self.runs:
            return "d", "never recorded"
        if not self.current():
            edited = "loop profile repinned" if self.profile_time > self.body_time else "harness edited"
            return "d", f"{edited} after last run"
        return "a", f"passed {slowest or 0:.0f}s"


# --------------------------------------------------------------------------
# Inputs.


def blame_times(path: str, start: int, end: int, cache: dict[str, list[int]]) -> int:
    """Newest author time among lines ``start..end`` (1-based) of ``path``."""

    if path not in cache:
        output = subprocess.run(
            ["git", "blame", "-w", "--line-porcelain", "--", path],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        cache[path] = [int(line.split()[1]) for line in output.splitlines() if line.startswith("author-time ")]
    times = cache[path][start - 1:end]
    return max(times) if times else 0


def profile_lines() -> dict[str, tuple[int, int, set[str]]]:
    """Profile -> (first line, last line, names of the loop tables it references)."""

    tree = ast.parse((ROOT / OPTIONS).read_text())
    spans: dict[str, tuple[int, int, set[str]]] = {}
    for node in tree.body:
        value = node.value if isinstance(node, (ast.Assign, ast.AnnAssign)) else None
        if isinstance(value, ast.Dict):
            for key, item in zip(value.keys, value.values):
                if isinstance(key, ast.Constant) and isinstance(key.value, str):
                    names = {name.id for name in ast.walk(item) if isinstance(name, ast.Name)}
                    spans[key.value] = (item.lineno, item.end_lineno or item.lineno, names)
    tables = {
        target.id: (node.lineno, node.end_lineno or node.lineno)
        for node in tree.body if isinstance(node, ast.Assign)
        for target in node.targets if isinstance(target, ast.Name)
    }
    return {
        name: (start, end, {table for table in names if table in tables})
        for name, (start, end, names) in spans.items()
    } | {f"table:{name}": (*span, set()) for name, span in tables.items()}


def read_transcripts(paths: list[Path]) -> dict[str, list[Run]]:
    runs: dict[str, list[Run]] = {}
    for path in paths:
        text = path.read_text(errors="replace")
        when = path.stat().st_mtime
        starts = [(match.start(), match.group(1)) for match in CHECK.finditer(text)]
        for index, (offset, name) in enumerate(starts):
            end = starts[index + 1][0] if index + 1 < len(starts) else len(text)
            block = text[offset:end]
            result = RESULT.search(block)
            seconds = TIME.search(block)
            if result is None:
                outcome = "oom" if OOM.search(block) else "unfinished"
            elif result.group(1) != "SUCCESSFUL":
                outcome = "failed"
            else:
                # A pass with an unsatisfied cover proves nothing on that branch.
                outcome = "vacuous" if cover_shortfall(block) else "passed"
            runs.setdefault(name, []).append(
                Run(outcome, float(seconds.group(1)) if seconds and result else None, when, str(path))
            )
    return runs


def inventory(transcripts: list[Path]) -> tuple[list[Row], list[formal_plan.Batch]]:
    plan = formal_plan.batches(ROOT, "full")
    batch_of = {(batch.selection.package, name): index for index, batch in enumerate(plan) for name in batch.names}
    runs = read_transcripts(transcripts)
    profiles = profile_lines()
    blame: dict[str, list[int]] = {}
    rows: list[Row] = []
    for planned in formal_plan.harnesses(ROOT):
        harness = planned.harness
        row = Row(planned, batch_of[(planned.selection.package, planned.qualified[0])])
        raw = (ROOT / harness.path).read_text()
        syntax = rust_syntax(raw)
        offset = sum(len(line) + 1 for line in syntax.split("\n")[: harness.line - 1])
        opening = syntax.index("{", offset)
        closing = _balanced_end(syntax, opening, "{", "}") or len(syntax)
        body = syntax[opening:closing]
        body_end = raw[:closing].count("\n") + 1
        lines = raw.split("\n")
        top = harness.line - 1
        while top > 0 and BLOCK_LINE.match(lines[top - 1]):
            top -= 1
        block = "\n".join(lines[top:harness.line])
        row.body_time = blame_times(harness.path, harness.line, body_end, blame)
        row.symbolic = len(SYMBOLIC.findall(body))
        unwinds = UNWIND.findall(block)
        row.unwind = int(unwinds[-1]) if unwinds else None
        row.heap = HEAP.search(body) is not None
        row.calls = tuple(sorted(set(CALL.findall(body)) - GENERIC_CALLS))
        profile = planned.selection.profile
        if profile and PROFILES[profile].package is not None and PROFILES[profile].loops:
            start, end, tables = profiles[profile]
            spans = [(start, end), *(profiles[f"table:{table}"][:2] for table in tables)]
            row.profile_time = max(blame_times(OPTIONS, a, b, blame) for a, b in spans)
        # A harness keeps its function name when a module or crate moves it;
        # function names are unique across the plan, so a moved run still counts.
        row.runs = [run for name, found in runs.items() if name.rsplit("::", 1)[-1] == harness.name for run in found]
        rows.append(row)
    return rows, plan


def candidate_groups(rows: list[Row]) -> dict[str, int]:
    """Harness -> group number for harnesses sharing package, obligations and calls."""

    groups: dict[tuple, list[Row]] = {}
    for row in rows:
        key = (row.planned.selection.package, tuple(sorted(a.value for a in row.planned.harness.values("obligation"))),
               row.calls)
        groups.setdefault(key, []).append(row)
    numbered = [members for members in groups.values() if len(members) > 1 and members[0].calls]
    return {row.name: number for number, members in enumerate(numbered, 1) for row in members}


# --------------------------------------------------------------------------
# Obligations.


def obligation_report(rows: list[Row], ledger: dict) -> list[tuple[str, str, str]]:
    """(obligation, verdict, detail) for every kernel-proved obligation whose
    marker is wrong or that is not backed by a class ``a`` run of every
    harness serving it. Verdicts: ``needs rerun`` (marked, still unbacked),
    ``mark needs_rerun`` (unmarked and unbacked) and ``clear marker`` (marked,
    now backed)."""

    classes = {row.planned.harness.name: row.classify()[0] for row in rows}
    report = []
    for obligation in ledger["obligations"]:
        if obligation["status"] != "kernel_proved":
            continue
        harnesses = obligation.get("proof_harnesses", [])
        unbacked = sorted(name for name in harnesses if classes.get(name) != "a")
        marked = obligation.get("run_evidence") == formal_ledger.NEEDS_RERUN
        detail = ", ".join(f"{name} ({classes.get(name, '?')})" for name in unbacked)
        if unbacked:
            report.append((obligation["id"], "needs rerun" if marked else "mark needs_rerun", detail))
        elif marked:
            report.append((obligation["id"], "clear marker", f"{len(harnesses)} harness(es) in class a"))
    return report


# --------------------------------------------------------------------------
# Slots.


def slots(rows: list[Row], plan: list[formal_plan.Batch], first: set[int],
          then: set[int] | frozenset[int] = frozenset()) -> list[list[int]]:
    """Batches packed into slots, in order: ``first``; batches holding a ``d``
    harness (a run decides them); ``then`` (for example the batches of fixed
    ``b`` harnesses, which are ``d`` until their first run); the rest; batches
    holding a ``b`` harness last, because they need their fix before a run can
    finish. Packing is first-fit within each of those groups, so a group never
    borrows an earlier slot."""

    classes: dict[int, set[str]] = {index: set() for index in range(len(plan))}
    for row in rows:
        classes[row.batch].add(row.classify()[0])

    def group(index: int) -> int:
        if index in first:
            return 0
        if index in then:
            return 2
        return 4 if "b" in classes[index] else 1 if "d" in classes[index] else 3

    packed: list[list[int]] = []
    used: list[float] = []
    for current in range(5):
        opened = len(packed)
        for index in (i for i in range(len(plan)) if group(i) == current):
            need = min(batch_cost(rows, index), KANI_BATCH_TIMEOUT_CEILING_SECONDS + BATCH_OVERHEAD_SECONDS)
            slot = next((slot for slot in range(opened, len(packed)) if used[slot] + need <= SLOT_SECONDS), None)
            if slot is None:
                packed.append([])
                used.append(0.0)
                slot = len(packed) - 1
            packed[slot].append(index)
            used[slot] += need
    return packed


def batch_cost(rows: list[Row], index: int, worst: bool = False) -> float:
    return BATCH_OVERHEAD_SECONDS + sum(row.estimate(worst) for row in rows if row.batch == index)


# --------------------------------------------------------------------------
# Output.


def first_batches(rows: list[Row], texts: list[str]) -> set[int]:
    """Batches holding a harness whose name, profile or obligation contains one of ``texts``."""

    def named(row: Row) -> bool:
        facets = [row.name, row.planned.selection.profile, *(a.value for a in row.planned.harness.values("obligation"))]
        return any(text in facet for text in texts for facet in facets)

    return {row.batch for row in rows if named(row)}


def report(rows: list[Row], plan: list[formal_plan.Batch], table: bool, first: set[int] | None,
           then: set[int] | frozenset[int] = frozenset()) -> None:
    classes = {row.name: row.classify() for row in rows}
    counts = {name: sum(1 for value, _ in classes.values() if value == name) for name in "abd"}
    print(f"harnesses {len(rows)}  batches {len(plan)}  " + "  ".join(f"{k}={v}" for k, v in counts.items()))
    deadline = KANI_BATCH_TIMEOUT_CEILING_SECONDS
    over = [index for index in range(len(plan)) if batch_cost(rows, index, worst=True) > deadline]
    print(f"batches whose slowest recorded passes exceed the {deadline}s deadline: {over}")
    groups = candidate_groups(rows)
    print(f"duplicate-candidate groups (same package, obligations, calls): {len(set(groups.values()))}")
    if table:
        print("\n| # | package | harness | obligations | profile/tier | batch | last run | cost | class |")
        print("|---|---|---|---|---|---|---|---|---|")
        for number, row in enumerate(rows, 1):
            planned = row.planned
            obligations = "<br>".join(a.value for a in planned.harness.values("obligation"))
            selection = "/".join(filter(None, (planned.selection.profile, planned.tier if planned.tier != "pr" else "")))
            last = row.last
            seen = "never" if last is None else (
                f"{last.result} {last.seconds:.0f}s" if last.seconds is not None else last.result
            )
            value, why = classes[row.name]
            group = f" G{groups[row.name]}" if row.name in groups else ""
            print(f"| {number} | {planned.selection.package} | `{row.name}` | {obligations} | {selection or '-'} "
                  f"| {row.batch} | {seen} | {row.cost()} | {value}: {why}{group} |")
    if first is not None:
        for number, members in enumerate(slots(rows, plan, first, then), 1):
            total = sum(min(batch_cost(rows, i), KANI_BATCH_TIMEOUT_CEILING_SECONDS + BATCH_OVERHEAD_SECONDS)
                        for i in members)
            print(f"\n# slot {number}: {len(members)} batches, estimated {total / 60:.0f} min")
            for index in members:
                print(f"{index}\t{' '.join(plan[index].argv())}")


def self_test() -> None:
    """Decision table of ``Row.classify``.

    Causes: C1 no run; C2 last run unfinished/oom/failed; C3 slowest pass at
    least half the deadline; C4 body newer than the last run; C5 package-bound
    profile newer than the last run; C6 a fresh fast pass; C7 the newest run
    passed with an unsatisfied cover.
    Effects: C1 => d; C2 => b; C3 => b; C4 => d; C5 => d; C6 => a; C7 => b.
    b outranks d; C2 and C3 read the runs of the current text when there are
    any. The transcript (T1, T2) and obligation (O1-O5) rows follow below.
    """

    planned = formal_plan.Planned(None, formal_plan.Selection("p"), "pr", ("h",))  # type: ignore[arg-type]

    def row(*runs: Run, body: int = 0, profile: int = 0) -> str:
        return Row(planned, 0, body_time=body, profile_time=profile, runs=list(runs)).classify()[0]

    fast = Run("passed", 5.0, 100.0, "t")
    assert row() == "d", "C1"
    for outcome in ("unfinished", "oom", "failed"):
        assert row(fast, Run(outcome, None, 200.0, "t")) == "b", ("C2", outcome)
    assert row(Run("passed", float(AT_RISK_SECONDS), 100.0, "t")) == "b", "C3"
    assert row(fast, body=150) == "d", "C4"
    assert row(fast, profile=150) == "d", "C5"
    assert row(fast, body=50, profile=50) == "a", "C6"
    assert row(Run("unfinished", None, 100.0, "t"), body=150) == "b", "b outranks d"
    slow_then_fixed = (Run("passed", float(AT_RISK_SECONDS), 50.0, "t"), Run("passed", 5.0, 200.0, "t"))
    assert row(*slow_then_fixed, body=100) == "a", "only runs of the current text measure it"
    assert row(Run("unfinished", None, 200.0, "t"), fast) == "b", "C2 is the newest run, not any run"
    assert row(fast, Run("vacuous", 5.0, 200.0, "t")) == "b", "C7 an unsatisfied cover is not a pass"

    # Transcript reading: a SUCCESSFUL block with an unsatisfied cover is vacuous.
    import tempfile  # noqa: PLC0415 - only the self-test builds fixtures

    with tempfile.TemporaryDirectory() as temporary:
        transcript = Path(temporary) / "kani.log"
        transcript.write_text(
            "Checking harness m::covered...\n ** 0 of 3 failed\n ** 2 of 2 cover properties satisfied\n"
            "VERIFICATION:- SUCCESSFUL\nVerification Time: 1.5s\n"
            "Checking harness m::vacuous...\n ** 0 of 3 failed\n ** 1 of 2 cover properties satisfied\n"
            "VERIFICATION:- SUCCESSFUL\nVerification Time: 2s\n"
        )
        runs = read_transcripts([transcript])
        assert [run.result for run in runs["m::covered"]] == ["passed"], "T1"
        assert [run.result for run in runs["m::vacuous"]] == ["vacuous"], "T2"

    # Obligations: O1 every harness in a, unmarked => nothing; O2 one harness
    # not in a, unmarked => mark; O3 the same, marked => needs rerun; O4 every
    # harness in a, marked => clear; O5 not kernel_proved => never listed.
    harnesses = {"fresh": fast, "stale": None}

    def planned_row(name: str) -> Row:
        harness = type("H", (), {"name": name})()
        built = Row(formal_plan.Planned(harness, formal_plan.Selection("p"), "pr", (name,)), 0)  # type: ignore[arg-type]
        built.runs = [harnesses[name]] if harnesses[name] else []
        return built

    ledger = {"obligations": [
        {"id": "o.one", "status": "kernel_proved", "proof_harnesses": ["fresh"]},
        {"id": "o.two", "status": "kernel_proved", "proof_harnesses": ["fresh", "stale"]},
        {"id": "o.three", "status": "kernel_proved", "proof_harnesses": ["stale"], "run_evidence": "needs_rerun"},
        {"id": "o.four", "status": "kernel_proved", "proof_harnesses": ["fresh"], "run_evidence": "needs_rerun"},
        {"id": "o.five", "status": "model_linked", "proof_harnesses": ["stale"]},
    ]}
    verdicts = {key: verdict for key, verdict, _ in obligation_report([planned_row("fresh"), planned_row("stale")], ledger)}
    assert verdicts == {"o.two": "mark needs_rerun", "o.three": "needs rerun", "o.four": "clear marker"}, verdicts

    # Slots: S1 group order is first, d, then, a, b; S2 a later group never
    # borrows an earlier group's slot.
    fresh, stale, also_fresh = planned_row("fresh"), planned_row("stale"), planned_row("fresh")
    stale.batch, also_fresh.batch = 1, 2
    plan = [formal_plan.Batch(formal_plan.Selection("p")) for _ in range(3)]
    assert slots([fresh, stale, also_fresh], plan, set(), {2}) == [[1], [2], [0]], "S1 S2"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, fromfile_prefix_chars="@", allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("transcripts", nargs="*", type=Path, help="Kani transcripts (or @file listing them)")
    parser.add_argument("--table", action="store_true", help="print the per-harness Markdown table")
    parser.add_argument("--slots", action="store_true", help="print the batches packed into build slots")
    parser.add_argument("--first", action="append", default=[], metavar="TEXT",
                        help="schedule first the batches of harnesses whose name, profile or obligation contains TEXT")
    parser.add_argument("--then", action="append", default=[], metavar="TEXT",
                        help="schedule right after the class d batches the batches of harnesses whose name, "
                             "profile or obligation contains TEXT")
    parser.add_argument("--slot", type=int, metavar="N",
                        help="print only slot N's batches, one `package argv...` line each (formal_plan's format)")
    parser.add_argument("--obligations", action="store_true",
                        help="list kernel-proved obligations not backed by a class a run, and wrong markers")
    parser.add_argument("--self-test", action="store_true", help="run the classification decision table")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        self_test()
        print("formal_inventory self-test passed")
        return 0
    missing = [str(path) for path in arguments.transcripts if not path.is_file()]
    if missing:
        parser.error(f"not a transcript file: {', '.join(missing)}")
    rows, plan = inventory(arguments.transcripts)
    if arguments.slot is not None:
        first = first_batches(rows, arguments.first)
        packed = slots(rows, plan, first, first_batches(rows, arguments.then) - first)
        if not 1 <= arguments.slot <= len(packed):
            parser.error(f"--slot must be 1..{len(packed)}")
        print("\n".join(" ".join(plan[index].argv()) for index in packed[arguments.slot - 1]))
        return 0
    first = first_batches(rows, arguments.first)
    report(rows, plan, arguments.table, first if arguments.slots else None,
           first_batches(rows, arguments.then) - first)
    if arguments.obligations:
        listed = obligation_report(rows, formal_ledger.load(ROOT))
        print(f"\nkernel-proved obligations to act on: {len(listed)}")
        for obligation, verdict, detail in listed:
            print(f"{verdict}\t{obligation}\t{detail}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
