"""Every Session state machine keeps one decision point; two ratchets that only shrink.

ADR-0089 D6 and ADR-0090 D7 (plan 9.13, W27). The Session aggregate decides
every transition of the Session and of its realization, and one executor --
`awaken-session-realization` -- carries out realization effects. Two measures of
how far the code still is from that, both counted from production code
(comments, string literals and `cfg(test)` code are not scanned) and held at or
below `single_owner_baseline.json`:

* `state-construction`: a site outside the state contract that constructs a
  `SessionExecutionState` or `SessionEnvironmentState` variant, per
  `crate::Enum`. No other crate may construct either: the realization reports
  observations and the aggregate's transition builds the state (ADR-0089 D1).
  Reading a state is not constructing one: a pattern (a
  `match` arm, `if let`/`let ... else`, the pattern of `matches!`) and an
  `==`/`!=` comparison are not counted. Importing the variants
  (`use Enum::*` or `use Enum::{..}`) is counted, because every later bare
  variant would be a construction this rule cannot see.
* `environment-rewrite`: a site outside the state contract that rewrites an
  existing `SessionEnvironmentState`, per crate: a mutable borrow of the type
  (`&mut SessionEnvironmentState`), a call of one of its undecided data laws
  on an `.environment` value (a `&mut self` method of `impl
  SessionEnvironmentState` in the state contract whose body does not decide
  through `transition_environment`; the list is read from the contract), or an
  assignment to `.environment` of another state value. A law that decides
  through the transition function (such as `deliver_legacy_binding`) and the
  aggregate's `SessionEnvironmentOperations` (`session.environment().law(..)`)
  are the sanctioned ways to change the state.
* `terminal-cleanup-orchestration`: a production function outside the
  realization driver (`awaken-session-realization-driver`) that orchestrates
  terminal cleanup, per crate. A function orchestrates when it calls two or
  more distinct terminal cleanup steps -- the terminal cleanup Control
  operations (`TERMINAL_CLEANUP_CONTROL_STEPS`) and the driver's entry points
  (`TERMINAL_CLEANUP_DRIVER_ENTRIES`) -- or calls a selection read
  (`claim_next_terminal_cleanup`, `terminal_cleanup_work`) inside a loop (a
  recovery sweep). A transport route or a forwarding implementation calls one
  step; a scheduler calls one driver entry. ADR-0089 D1: terminal cleanup
  orchestration (absence handling, retirement, revocation, the recovery sweep)
  exists once, in the driver.
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
    "SessionEnvironmentState": frozenset(),
}
ENVIRONMENT_STATE = "SessionEnvironmentState"
REALIZATION_OWNERS = frozenset({REALIZATION_CRATE, "awaken-session-realization-contract", "awaken-provisioning-contract"})
PROVIDER_CONTEXT = "sandbox"
REALIZATION_MODULES = frozenset({
    "provisioning", "environment_lifecycle", "environment_continuation", "terminal_reconciliation",
})
REALIZATION_WORDS = frozenset({"realization", "realizer", "realized", "realize"})
DRIVER_CRATE = "awaken-session-realization-driver"
TERMINAL_CLEANUP_SELECTION_READS = frozenset({"claim_next_terminal_cleanup", "terminal_cleanup_work"})
# The Control's preparation authorization and the preparation and disposal
# records share their names with the aggregate laws a Control implementation
# calls; they are left out so a law call is never read as a Control step.
TERMINAL_CLEANUP_CONTROL_STEPS = TERMINAL_CLEANUP_SELECTION_READS | frozenset({
    "terminal_repository_publication_command", "record_terminal_repository_publication_receipt",
    "record_terminal_repository_publication_rejection", "authorize_terminal_cleanup_disposal",
})
TERMINAL_CLEANUP_DRIVER_ENTRIES = frozenset({
    "drive_session_terminal_cleanup", "reconcile_terminal_cleanup", "recover_terminal_cleanup_assignments",
})

RULES = (
    Rule("state-construction", "construct the state through the Session aggregate's transition in the state contract "
         "(ADR-0090 D7), or map the observation in the realization (ADR-0089 D6)"),
    Rule("environment-rewrite", "change the Environment state through a law that decides through "
         "transition_environment, or through the aggregate's SessionEnvironmentOperations (ADR-0089 D1)"),
    Rule("terminal-cleanup-orchestration", "orchestrate terminal cleanup only in awaken-session-realization-driver "
         "(reconcile_terminal_cleanup / recover_terminal_cleanup_assignments); elsewhere call one driver entry or "
         "serve one Control step (ADR-0089 D1)"),
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


_MUTABLE_ENVIRONMENT = re.compile(r"&\s*mut\s+(?:\w+\s*::\s*)*" + ENVIRONMENT_STATE + r"\b")
_ENVIRONMENT_ASSIGNMENT = re.compile(r"\.\s*environment\s*=(?![=>])([^;]*);")
_COPIED_STATE = re.compile(r"^\s*[\w.()&*\s]*\.\s*environment\s*(?:\.\s*clone\s*\(\s*\)\s*)?$")


def undecided_environment_laws(contract_syntax: dict[Path, str]) -> frozenset[str]:
    """`&mut self` methods of `impl SessionEnvironmentState` that do not decide through the transition."""
    laws = set()
    for syntax in contract_syntax.values():
        for block in re.finditer(r"\bimpl\s+" + ENVIRONMENT_STATE + r"\s*\{", syntax):
            end = _crate_boundary_workspace._balanced_end(syntax, block.end() - 1, "{", "}") or len(syntax)
            body = syntax[block.end():end]
            for method in re.finditer(r"\bfn\s+(\w+)\s*(?:<[^>]*>)?\s*\(\s*&\s*mut\s+self\b", body):
                opening = body.find("{", method.end())
                closing = _crate_boundary_workspace._balanced_end(body, opening, "{", "}") or len(body)
                if "transition_environment" not in body[opening:closing]:
                    laws.add(method.group(1))
    return frozenset(laws)


def environment_rewrites(syntax: str, laws: frozenset[str]) -> list[int]:
    """Offsets of every Environment-state rewrite in one file's code (literals already blanked)."""
    found = [match.start() for match in _MUTABLE_ENVIRONMENT.finditer(syntax)]
    if laws:
        law_call = re.compile(r"\.\s*environment\s*\.\s*(?:" + "|".join(sorted(laws)) + r")\s*\(")
        found += [match.start() for match in law_call.finditer(syntax)]
    for match in _ENVIRONMENT_ASSIGNMENT.finditer(syntax):
        value = match.group(1)
        if ENVIRONMENT_STATE in value or _COPIED_STATE.match(value):
            found.append(match.start())
    return sorted(found)


_FUNCTION = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{;]*?>)?\s*\(")
_STEP_CALL = re.compile(r"(?:\.|::|\b)\s*(" + "|".join(sorted(TERMINAL_CLEANUP_CONTROL_STEPS
                                                              | TERMINAL_CLEANUP_DRIVER_ENTRIES)) + r")\s*\(")
_LOOP = re.compile(r"\b(?:loop|while|for)\b")


def _function_bodies(syntax: str) -> list[tuple[str, int, int]]:
    """(name, body start, body end) of every function with a body in one file's code."""
    found = []
    for match in _FUNCTION.finditer(syntax):
        signature_end = _crate_boundary_workspace._balanced_end(syntax, match.end() - 1, "(", ")")
        if signature_end is None:
            continue
        opening = signature_end
        while opening < len(syntax) and syntax[opening] not in "{;":
            opening += 1
        if opening >= len(syntax) or syntax[opening] == ";":
            continue
        closing = _crate_boundary_workspace._balanced_end(syntax, opening, "{", "}") or len(syntax)
        found.append((match.group(1), opening, closing))
    return found


def _loop_spans(syntax: str, start: int, end: int) -> list[tuple[int, int]]:
    spans = []
    for match in _LOOP.finditer(syntax, start, end):
        opening = syntax.find("{", match.end(), end)
        if opening < 0:
            continue
        closing = _crate_boundary_workspace._balanced_end(syntax, opening, "{", "}") or end
        spans.append((opening, closing))
    return spans


def terminal_cleanup_orchestrations(syntax: str) -> list[int]:
    """Offsets of every function in one file's code that orchestrates terminal cleanup."""
    found = []
    for name, start, end in _function_bodies(syntax):
        calls = [(match.group(1), match.start(1)) for match in _STEP_CALL.finditer(syntax, start, end)
                 if not syntax[max(0, match.start(1) - 3):match.start(1)].rstrip().endswith("fn")]
        steps = {step for step, _ in calls}
        loops = _loop_spans(syntax, start, end)
        swept = any(step in TERMINAL_CLEANUP_SELECTION_READS and any(low < offset < high for low, high in loops)
                    for step, offset in calls)
        if len(steps) >= 2 or swept:
            found.append(start)
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
    counts: Counts = {"state-construction": Counter(), "environment-rewrite": Counter(),
                      "terminal-cleanup-orchestration": Counter(), "realization-code": Counter()}
    places: Places = {"state-construction": defaultdict(list), "environment-rewrite": defaultdict(list),
                      "terminal-cleanup-orchestration": defaultdict(list), "realization-code": defaultdict(list)}
    laws = frozenset().union(*(undecided_environment_laws(crate.syntax)
                               for crate in sources if crate.name == STATE_CONTRACT))
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
            if crate.name != STATE_CONTRACT:
                for offset in environment_rewrites(syntax, laws):
                    counts["environment-rewrite"][crate.name] += 1
                    places["environment-rewrite"][crate.name].append(
                        f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
            if crate.name != DRIVER_CRATE:
                for offset in terminal_cleanup_orchestrations(syntax):
                    counts["terminal-cleanup-orchestration"][crate.name] += 1
                    places["terminal-cleanup-orchestration"][crate.name].append(
                        f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
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
            for error in _architecture_ratchet.compare(
                RULES, counts, places, baseline, baseline_path.name,
                lower_advice=f"python3 scripts/ci/{Path(__file__).name} --tighten to lower {baseline_path.name} in this commit")]


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                  | effect |
    | S1   | variant as a value: argument, assignment, struct field, `.as_str()`    | counted |
    | S2   | variant in a match arm, `|` alternative, guard, `if let`, `let..else`, `matches!` pattern | not counted |
    | S3   | `==` / `!=` operand on either side, qualified path                     | not counted |
    | S4   | `use Enum as Alias` then `Alias::Variant`; `use Enum::*` / `use Enum::{..}` | counted |
    | S5   | the state contract (no other crate, the realization included, is exempt) | not counted |
    | S6   | module path segment `provisioning`, or a word `realization` in a segment | realization-named |
    | S7   | `unrealized_x` style segment without the word, `lib.rs`                 | not realization-named |
    | S8   | realization module in the realization contract / a sandbox provider    | not counted |

    | Rule | environment-rewrite cause                                              | effect |
    | W1   | `&mut SessionEnvironmentState` (qualified or not)                      | counted |
    | W2   | an undecided `&mut self` law called on an `.environment` value         | counted |
    | W3   | `.environment = <SessionEnvironmentState value>` or `= other.environment(.clone())` | counted |
    | W4   | a law whose body decides through `transition_environment`, called on `.environment` | not counted |
    | W5   | the aggregate wrapper `session.environment().law(..)`, a read, `&SessionEnvironmentState` | not counted |
    | W6   | `.environment = <non-state value>` (another type's field), `==` comparison | not counted |
    | W7   | the state contract itself                                              | not counted |

    | Rule | terminal-cleanup-orchestration cause                                   | effect |
    | O1   | one function: a work read, then a driver entry (the old executor reconcile) | counted |
    | O2   | a claim read inside `loop`/`while`/`for` (a recovery sweep)            | counted |
    | O3   | one function: two distinct Control steps (claim, then work read)       | counted |
    | O4   | a transport route or forwarder calling one Control step                | not counted |
    | O5   | a scheduler calling one driver entry, also inside a loop               | not counted |
    | O6   | a step declaration (`fn terminal_cleanup_work(`) or a same-named aggregate law | not counted |
    | O7   | the realization driver crate                                           | not counted |
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
    assert all(not allowed for allowed in STATE_ENUMS.values()), "S5 no crate besides the contract is exempt"

    contract_code = _crate_boundary_workspace.rust_syntax(
        "impl SessionEnvironmentState {\n"
        "    pub fn set_resident(&mut self, binding: String) { *self = Self::Unmaterialized; }\n"
        "    pub fn deliver_legacy_binding(&mut self, b: Option<&str>) -> bool {\n"
        "        transition_environment(f, e).is_ok()\n"
        "    }\n"
        "    pub fn binding(&self) -> Option<&str> { None }\n"
        "}\n"
        "impl Other { pub fn mark_idle(&mut self) {} }\n"
    )
    laws = undecided_environment_laws({Path("environment.rs"): contract_code})
    assert laws == {"set_resident"}, f"W2/W4 law list {laws}"
    rewrites = (
        "fn a(e: &mut SessionEnvironmentState) {}\n"                                     # W1
        "fn b(e: &mut awaken_session_state_contract::SessionEnvironmentState) {}\n"      # W1
        "fn c(s: &mut S) { s.environment.set_resident(x); }\n"                           # W2
        "fn d(s: &mut S) { s.environment = SessionEnvironmentState::from_legacy_binding(None); }\n"  # W3
        "fn e(s: &mut S, o: &S) { s.environment = o.environment.clone(); }\n"            # W3
    )
    found = environment_rewrites(_crate_boundary_workspace.rust_syntax(rewrites), laws)
    assert [rewrites.count("\n", 0, offset) + 1 for offset in found] == [1, 2, 3, 4, 5], f"W1-W3 {found}"
    allowed = (
        "fn f(p: &mut P) { p.environment.deliver_legacy_binding(b); }\n"                 # W4
        "fn g(s: &mut A) { s.environment().record_checkpoint(r, now); }\n"               # W5
        "fn h(e: &SessionEnvironmentState) -> bool { e.binding().is_some() }\n"           # W5
        "fn i(p: &mut Q) { p.environment = p.environment.with_preparation_authorization(a); }\n"  # W6
        "fn j(s: &mut Spec) { s.environment = Some(kind); }\n"                           # W6
        "fn k(a: &S, b: &S) -> bool { a.environment == b.environment }\n"                 # W6
    )
    assert environment_rewrites(_crate_boundary_workspace.rust_syntax(allowed), laws) == [], "W4-W6"
    orchestration = _crate_boundary_workspace.rust_syntax(
        "async fn reconcile(c: &C) {\n"                                                  # O1
        "    match c.terminal_cleanup_work(s, l).await { Ok(Some(_)) => driver::drive_session_terminal_cleanup(s).await, _ => {} }\n"
        "}\n"
        "async fn sweep(c: &C) {\n"                                                      # O2
        "    loop { let next = c.claim_next_terminal_cleanup(t, after).await; reconcile(next).await; }\n"
        "}\n"
        "async fn both(c: &C) { c.claim_next_terminal_cleanup(t, None).await; c.terminal_cleanup_work(s, l).await; }\n"  # O3
        "async fn route(c: &C) { c.terminal_cleanup_work(s, l).await }\n"                # O4
        "async fn terminal_cleanup_work(&self) { self.client.terminal_cleanup_work(s, l).await }\n"  # O4/O6
        "async fn schedule(c: &C) { for t in targets { recover_terminal_cleanup_assignments(t, c).await; } }\n"  # O5
        "async fn law(s: &mut S) { s.authorize_terminal_cleanup_effect(e); s.cleanup().record_terminal_cleanup_preparation(p); }\n"  # O6
    )
    found = [orchestration.count("\n", 0, offset) + 1 for offset in terminal_cleanup_orchestrations(orchestration)]
    assert found == [1, 4, 7], f"O1-O6 {found}"
    driver = _architecture_ratchet.CrateSource(
        DRIVER_CRATE, {}, Path("/w/d"),
        {Path("/w/d/src/recovery.rs"): "async fn sweep(c: &C) { loop { c.claim_next_terminal_cleanup(t, a).await; } }\n"},
    )
    driven, _ = measure((driver,), Path("/w"))
    assert driven["terminal-cleanup-orchestration"] == {}, f"O7 {driven}"
    assert is_realization_module(Path("host/session/environment_lifecycle/renewal.rs")), "S6 named"
    assert is_realization_module(Path("provisioning.rs")) and is_realization_module(Path("k8s/realization/mod.rs")), \
        "S6 word"
    assert is_realization_module(Path("worker_resolver/session_realization.rs")), "S6 word in segment"
    assert not is_realization_module(Path("lib.rs")) and not is_realization_module(Path("unrealizedx.rs")), "S7"
    assert realization_owner("awaken-session-realization-contract", {}), "S8 contract"
    assert realization_owner("awaken-sandbox-local", {"bounded_context": "sandbox"}), "S8 provider"
    assert not realization_owner("awaken-session-executor", {"bounded_context": "session"}), "S8 other"
    assert module_of(Path("host/session/environment_lifecycle/mod.rs")) == ("host", "session", "environment_lifecycle")
    # S9 causes: historical transitional classification / completed extraction;
    # effect: only explicitly classified in-place R code is excluded. The
    # fixture keeps historical behavior covered without a blanket Host exemption.
    transitional = {("host", "session", "environment_lifecycle"): _module_layer_fitness.REALIZATION}
    assert in_extracting_realization(
        "awaken-session-executor", Path("host/session/environment_lifecycle/renewal.rs"), transitional
    ), "S9 explicit transitional R module is excluded"
    assert not in_extracting_realization(
        "awaken-session-executor", Path("host/session/environment_lifecycle/renewal.rs")
    ), "S9 completed extraction has no implicit Host exemption"
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
        STATE_CONTRACT, {}, Path("/w/c"),
        {Path("/w/c/src/realization.rs"): "fn f() { g(SessionExecutionState::Idle) }\n",
         Path("/w/c/src/environment.rs"): contract_code
         + "fn own(s: &mut SessionEnvironmentState) { s.set_resident(b); }\n"},
    )
    realization = _architecture_ratchet.CrateSource(
        REALIZATION_CRATE, {}, Path("/w/r"),
        {Path("/w/r/src/sync.rs"): "fn s(p: &mut P) { p.environment.set_resident(b); }\n"},
    )
    counts, places = measure((source, contract, realization), Path("/w"))
    assert counts == {"state-construction": {"awaken-x::SessionExecutionState": 1},
                      "environment-rewrite": {REALIZATION_CRATE: 1},
                      "terminal-cleanup-orchestration": {},
                      "realization-code": {"awaken-x": 3, STATE_CONTRACT: 1}}, f"measure W2/W7 {counts}"
    assert places["state-construction"]["awaken-x::SessionExecutionState"] == ["x/src/realization.rs:2"], "places"
    print("_single_owner_fitness selftest: the decision tables hold")


if __name__ == "__main__":
    raise SystemExit(_architecture_ratchet.main(
        sys.argv[1:], prog=Path(__file__).name, description=__doc__, rules=RULES, measure=_measure_workspace,
        baseline_path=BASELINE, self_test=selftest))
