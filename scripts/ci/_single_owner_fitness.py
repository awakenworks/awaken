"""Every Session state machine keeps one decision point; two ratchets that only shrink.

ADR-0089 D6 and ADR-0090 D7 (plan 9.13, W27). The Session aggregate decides
every transition of the Session and of its realization, and one executor --
`awaken-session-realization` -- carries out realization effects. Two measures of
how far the code still is from that, both counted from production code
(comments, string literals and `cfg(test)` code are not scanned) and held at or
below `single_owner_baseline.json`:

* `state-construction`: a site outside the state contract that constructs a
  `SessionExecutionState` or `SessionEnvironmentState` variant, per
  `crate::Enum`. The realization's observation mapping may also construct the
  environment sub-state. Reading a state is not constructing one: a pattern (a
  `match` arm, `if let`/`let ... else`, the pattern of `matches!`) and an
  `==`/`!=` comparison are not counted. Importing the variants
  (`use Enum::*` or `use Enum::{..}`) is counted, because every later bare
  variant would be a construction this rule cannot see.
* `realization-code`: production lines (non-blank, comments excluded) in modules
  named for realization -- a module path segment in `REALIZATION_MODULES`, or one
  whose words include one of `REALIZATION_WORDS` -- per crate, outside the
  realization, its contract, the provisioning contract and the sandbox providers
  (crates of bounded context `sandbox`). Modules of `awaken-session-executor` that
  the module-layer guard classifies as `realization` are the realization still being
  extracted (plan slice R); the module-layer guard governs them, so moving code
  into them is consolidation, not growth. Lines may float below the baseline and
  must be re-recorded once they fall `LINE_SLACK` or more below it.

A count above its baseline is an error; see `_architecture_ratchet.py` for how a
fall is recorded (`python3 scripts/ci/_single_owner_fitness.py --write-baseline`).
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import _architecture_ratchet
import _crate_boundary_workspace
import _module_layer_fitness
from _architecture_ratchet import Counts, Places, Rule

BASELINE = Path(__file__).with_name("single_owner_baseline.json")
LINE_SLACK = 200

STATE_CONTRACT = "awaken-session-state-contract"
REALIZATION_CRATE = "awaken-session-realization"
# Enum -> crates allowed to construct its variants besides the state contract.
STATE_ENUMS = {
    "SessionExecutionState": frozenset(),
    "SessionEnvironmentState": frozenset({REALIZATION_CRATE}),
}
REALIZATION_OWNERS = frozenset({REALIZATION_CRATE, "awaken-session-realization-contract", "awaken-provisioning-contract"})
PROVIDER_CONTEXT = "sandbox"
REALIZATION_MODULES = frozenset({
    "provisioning", "environment_lifecycle", "environment_continuation", "terminal_reconciliation",
})
REALIZATION_WORDS = frozenset({"realization", "realizer", "realized", "realize"})

RULES = (
    Rule("state-construction", "construct the state through the Session aggregate's transition in the state contract "
         "(ADR-0090 D7), or map the observation in the realization (ADR-0089 D6)"),
    Rule("realization-code", "put realization behaviour in awaken-session-realization, its contract or a sandbox "
         "provider (ADR-0089 D6), not in a module named for realization elsewhere", slack=LINE_SLACK),
)

_PATTERN_MACRO = re.compile(r"\b(?:matches|assert_matches|debug_assert_matches)!\s*\(")
_QUALIFIER = re.compile(r"(?:\b\w+\s*::\s*)+$")


def _skip_space(syntax: str, index: int) -> int:
    while index < len(syntax) and syntax[index].isspace():
        index += 1
    return index


def _pattern_spans(syntax: str) -> list[tuple[int, int]]:
    """The pattern argument (after the first top-level comma) of every `matches!`-style macro."""
    spans = []
    for match in _PATTERN_MACRO.finditer(syntax):
        opening = match.end() - 1
        end = _crate_boundary_workspace._balanced_end(syntax, opening, "(", ")") or len(syntax)
        depth = 0
        for index in range(opening + 1, end):
            if syntax[index] in "([{":
                depth += 1
            elif syntax[index] in ")]}":
                depth -= 1
            elif syntax[index] == "," and depth == 0:
                spans.append((index, end))
                break
    return spans


def _is_read(syntax: str, start: int, end: int, patterns: list[tuple[int, int]]) -> bool:
    """Whether the variant path at `start:end` is a pattern or a comparison operand."""
    if any(low < start < high for low, high in patterns):
        return True
    after = _skip_space(syntax, end)
    if after < len(syntax) and syntax[after] in "({":
        closing = ")" if syntax[after] == "(" else "}"
        after = _skip_space(syntax, _crate_boundary_workspace._balanced_end(syntax, after, syntax[after], closing)
                            or len(syntax))
    if syntax.startswith(("==", "!="), after):
        return True
    while after < len(syntax) and syntax[after] in ")}]":
        after = _skip_space(syntax, after + 1)
    rest = syntax[after:after + 3]
    if rest.startswith("=>") or (rest.startswith("|") and not rest.startswith("||")) or re.match(r"if\b", rest):
        return True
    before = syntax[max(0, start - 400):start]
    if before.rstrip().endswith(("==", "!=")):
        return True
    binding = list(re.finditer(r"\blet\b", before))
    return bool(binding) and not re.search(r"[;{}]|(?<![=!<>])=(?![=>])", before[binding[-1].end():])


def state_constructions(syntax: str, allowed_enums: set[str]) -> list[tuple[str, int]]:
    """(enum, offset) of every variant construction in one file's code (literals already blanked)."""
    if not allowed_enums:
        return []
    names = {enum: enum for enum in allowed_enums}
    for match in re.finditer(r"\buse\b[^;]*?\b(" + "|".join(allowed_enums) + r")\s+as\s+(\w+)\s*[;,}]", syntax):
        names[match.group(2)] = match.group(1)
    patterns = _pattern_spans(syntax)
    found = []
    alternatives = "|".join(sorted(map(re.escape, names), key=len, reverse=True))
    for match in re.finditer(r"\b(" + alternatives + r")\s*::\s*([A-Z]\w*|\*|\{)", syntax):
        enum = names[match.group(1)]
        if match.group(2) in {"*", "{"}:
            statement = syntax.rfind(";", 0, match.start()) + 1
            if re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?use\b", syntax[statement:match.start()]):
                found.append((enum, match.start()))
            continue
        qualifier = _QUALIFIER.search(syntax, 0, match.start())
        start = qualifier.start() if qualifier else match.start()
        if not _is_read(syntax, start, match.end(), patterns):
            found.append((enum, match.start()))
    return found


def is_realization_module(relative: Path) -> bool:
    parts = [part for part in relative.with_suffix("").parts if part not in {"mod", "lib", "main"}]
    return any(part in REALIZATION_MODULES or REALIZATION_WORDS & set(part.split("_")) for part in parts)


def module_of(relative: Path) -> tuple[str, ...]:
    """Module path of a source file relative to `src/` (`lib.rs` is the root, `x/mod.rs` is `x`)."""
    parts = list(relative.with_suffix("").parts)
    if parts and parts[-1] in ("mod", "lib", "main"):
        parts.pop()
    return tuple(parts)


def in_extracting_realization(crate_name: str, relative: Path, table=None) -> bool:
    """A runtime-host module the module-layer guard classifies as `realization`."""
    if crate_name != _module_layer_fitness.CRATE:
        return False
    table = _module_layer_fitness.layer_table() if table is None else table
    return _module_layer_fitness.target_of(module_of(relative), table) == _module_layer_fitness.REALIZATION


def realization_owner(name: str, metadata: dict) -> bool:
    return name in REALIZATION_OWNERS or metadata.get("bounded_context") == PROVIDER_CONTEXT


def measure(sources: tuple[_architecture_ratchet.CrateSource, ...], root: Path) -> tuple[Counts, Places]:
    counts: Counts = {"state-construction": Counter(), "realization-code": Counter()}
    places: Places = {"state-construction": defaultdict(list), "realization-code": defaultdict(list)}
    for crate in sources:
        enums = {enum for enum, allowed in STATE_ENUMS.items() if crate.name != STATE_CONTRACT
                 and crate.name not in allowed}
        owner = realization_owner(crate.name, crate.metadata)
        layers = _module_layer_fitness.layer_table()
        for path, syntax in crate.syntax.items():
            relative = (path.relative_to(root) if root in path.parents else path).as_posix()
            for enum, offset in state_constructions(syntax, enums):
                key = f"{crate.name}::{enum}"
                counts["state-construction"][key] += 1
                places["state-construction"][key].append(f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
            source_root = crate.directory / "src"
            if (not owner and source_root in path.parents
                    and is_realization_module(path.relative_to(source_root))
                    and not in_extracting_realization(crate.name, path.relative_to(source_root), layers)):
                lines = sum(1 for line in syntax.splitlines() if line.strip())
                counts["realization-code"][crate.name] += lines
                places["realization-code"][crate.name].append(str(relative))
    return ({rule: dict(found) for rule, found in counts.items()},
            {rule: dict(found) for rule, found in places.items()})


def _measure_workspace(crates: Path = _crate_boundary_workspace.CRATES) -> tuple[Counts, Places]:
    return measure(_architecture_ratchet.crate_sources(crates), _crate_boundary_workspace.REPO_ROOT)


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    counts, places = _measure_workspace(crates)
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    return [f"single-owner: {error}"
            for error in _architecture_ratchet.compare(RULES, counts, places, baseline, baseline_path.name)]


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                  | effect |
    | S1   | variant as a value: argument, assignment, struct field, `.as_str()`    | counted |
    | S2   | variant in a match arm, `|` alternative, guard, `if let`, `let..else`, `matches!` pattern | not counted |
    | S3   | `==` / `!=` operand on either side, qualified path                     | not counted |
    | S4   | `use Enum as Alias` then `Alias::Variant`; `use Enum::*` / `use Enum::{..}` | counted |
    | S5   | the state contract; the realization for the environment enum              | not counted |
    | S6   | module path segment `provisioning`, or a word `realization` in a segment | realization-named |
    | S7   | `unrealized_x` style segment without the word, `lib.rs`                 | not realization-named |
    | S8   | realization module in the realization contract / a sandbox provider    | not counted |
    """
    both = set(STATE_ENUMS)
    code = (
        "fn f(s: &mut S) {\n"
        "    s.transition(SessionExecutionState::Idle);\n"                              # S1
        "    s.environment = awaken_session_state_contract::SessionEnvironmentState::Resident { binding };\n"  # S1
        "    let name = SessionExecutionState::Running.as_str();\n"                     # S1
        "    match s.execution { SessionExecutionState::Idle | SessionExecutionState::Running => {} _ => {} }\n"
        "    match e { SessionEnvironmentState::Resident { .. } if ready => {} Some(SessionExecutionState::Idle) => {} }\n"
        "    if let SessionEnvironmentState::Hibernated { .. } = e {}\n"
        "    let SessionEnvironmentState::Resident { binding, .. } = e else { return };\n"
        "    let ok = matches!(s.execution, SessionExecutionState::Idle | SessionExecutionState::Running);\n"
        "    if s.execution == SessionExecutionState::Idle && x != a::b::SessionExecutionState::Running {}\n"
        "    if SessionExecutionState::Running != s.execution {}\n"
        "}\n"
    )
    found = state_constructions(_crate_boundary_workspace.rust_syntax(code), both)
    assert [(enum, code.count("\n", 0, offset) + 1) for enum, offset in found] == [
        ("SessionExecutionState", 2), ("SessionEnvironmentState", 3), ("SessionExecutionState", 4),
    ], f"S1-S3 {found}"
    aliased = "use awaken_session_state_contract::SessionExecutionState as State;\nfn f() { g(State::Idle); }\n"
    assert [enum for enum, _ in state_constructions(aliased, both)] == ["SessionExecutionState"], "S4 alias"
    glob = "use awaken_session_state_contract::SessionEnvironmentState::*;\nuse x::SessionExecutionState::{Idle};\n"
    assert len(state_constructions(glob, both)) == 2, "S4 glob"
    assert state_constructions("fn f() { g(SessionExecutionState::Idle); }", set()) == [], "S5"
    assert is_realization_module(Path("host/session/environment_lifecycle/renewal.rs")), "S6 named"
    assert is_realization_module(Path("provisioning.rs")) and is_realization_module(Path("k8s/realization/mod.rs")), \
        "S6 word"
    assert is_realization_module(Path("worker_resolver/session_realization.rs")), "S6 word in segment"
    assert not is_realization_module(Path("lib.rs")) and not is_realization_module(Path("unrealizedx.rs")), "S7"
    assert realization_owner("awaken-session-realization-contract", {}), "S8 contract"
    assert realization_owner("awaken-sandbox-local", {"bounded_context": "sandbox"}), "S8 provider"
    assert not realization_owner("awaken-session-executor", {"bounded_context": "session"}), "S8 other"
    assert module_of(Path("host/session/environment_lifecycle/mod.rs")) == ("host", "session", "environment_lifecycle")
    assert in_extracting_realization("awaken-session-executor", Path("host/session/environment_lifecycle/renewal.rs")), \
        "S9 realizer-layer module of runtime-host is the extracting realization"
    assert not in_extracting_realization("awaken-session-executor", Path("host/session/realization_view.rs")), \
        "S9 host-layer module stays counted"
    assert not in_extracting_realization("awaken-session-application", Path("realization/control.rs")), \
        "S9 other crates stay counted"

    source = _architecture_ratchet.CrateSource(
        "awaken-x", {}, Path("/w/x"),
        {Path("/w/x/src/realization.rs"): "fn f() {\n    g(SessionExecutionState::Idle);\n}\n",
         Path("/w/x/src/other.rs"): "fn h() {}\n"},
    )
    contract = _architecture_ratchet.CrateSource(
        STATE_CONTRACT, {}, Path("/w/c"), {Path("/w/c/src/realization.rs"): "fn f() { g(SessionExecutionState::Idle) }\n"},
    )
    counts, places = measure((source, contract), Path("/w"))
    assert counts == {"state-construction": {"awaken-x::SessionExecutionState": 1},
                      "realization-code": {"awaken-x": 3, STATE_CONTRACT: 1}}, f"measure {counts}"
    assert places["state-construction"]["awaken-x::SessionExecutionState"] == ["x/src/realization.rs:2"], "places"
    print("_single_owner_fitness selftest: the decision tables hold")


if __name__ == "__main__":
    raise SystemExit(_architecture_ratchet.main(
        sys.argv[1:], prog=Path(__file__).name, description=__doc__, rules=RULES, measure=_measure_workspace,
        baseline_path=BASELINE, self_test=selftest))
