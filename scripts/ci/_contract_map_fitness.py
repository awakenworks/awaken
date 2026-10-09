"""Contract crates follow the target map and contract traits are admitted by their callers.

ADR-0091 D2, D4, D5 (plan 9.14, W28). Counted from production code (comments,
string literals and `cfg(test)` code are not scanned):

* `target-map` (a hard rule, not a ratchet): every crate of layer `contract` is
  a target contract of `TARGET_MAP` or is listed there as merging into one. A
  crate listed as merging that no longer exists is an error too, so the list of
  pending merges only shrinks. The map encodes plan 9.14: 26 target contracts,
  of which `awaken-session-contract` is the only new name.
* `contract-crates`: the number of contract crates.
* `optional-role`: a method declared in a contract trait, or an inherent method
  of a contract type, returning `Option<&dyn ...>` or `Option<Arc<dyn ...>>`,
  keyed `Owner::method`. A role is given at construction (D5), not discovered.
* `bundle-trait`: a `pub trait` in a contract crate that declares no method (a
  marker or a bundle of supertraits). Bundles belong in composition roots (D4).
* `unimplemented-trait`: a `pub trait` of a contract crate that no production
  code implements (`impl ... Trait ... for`), counting every crate except those
  of layer `tooling`. Every contract trait has a production implementation (D4).

Every ratchet is held at or below `contract_map_baseline.json`; see
`_architecture_ratchet.py` for how a fall is recorded
(`python3 scripts/ci/_contract_map_fitness.py --write-baseline`).
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import _architecture_ratchet
import _crate_boundary_workspace
from _architecture_ratchet import Counts, Places, Rule

BASELINE = Path(__file__).with_name("contract_map_baseline.json")

# Plan 9.14 target map: target contract -> (boundary, contracts that merge into it).
TARGET_MAP: dict[str, tuple[str, tuple[str, ...]]] = {
    "awaken-agent-contract": ("SDK release", ()),
    "awaken-tool-contract": ("SDK release", ()),
    "awaken-lease-contract": ("SDK release", ()),
    "awaken-time": ("SDK release", ()),
    "awaken-runtime-contract": ("SDK release", ("awaken-web-policy-contract",)),
    "awaken-session-types": ("Session", ()),
    "awaken-session-contract": ("Session", (
        "awaken-session-state-contract", "awaken-session-event-contract", "awaken-session-budget-contract",
        "awaken-session-dream-contract", "awaken-session-repository-contract",
    )),
    "awaken-session-configuration-contract": ("Session", ("awaken-session-resource-contract",
                                                          "awaken-session-mcp-contract")),
    "awaken-session-application-contract": ("Session", ()),
    "awaken-session-execution-contract": ("Session", ()),
    "awaken-session-realization-contract": ("Session", ("awaken-session-environment-contract",)),
    "awaken-deployment-contract": ("Deployment", ()),
    "awaken-dispatch-contract": ("Dispatch", ()),
    "awaken-executable-agent-contract": ("Dispatch", ()),
    "awaken-environment-contract": ("Environment and Worker", ()),
    "awaken-executable-environment-contract": ("Environment and Worker", ()),
    "awaken-acp-contract": ("Environment and Worker", ()),
    "awaken-agent-channel": ("Environment and Worker", ()),
    "awaken-sandbox-control": ("Environment and Worker", ()),
    "awaken-provisioning-contract": ("Environment and Worker", ("awaken-container-contract",)),
    "awaken-worker-contract": ("Environment and Worker", ()),
    "awaken-credential-contract": ("Access and resources", ()),
    "awaken-tenancy": ("Access and resources", ()),
    "awaken-service-auth-contract": ("Access and resources", ()),
    "awaken-data-governance-contract": ("Access and resources", ()),
    "awaken-resource-contract": ("Access and resources", ()),
}

RULES = (
    Rule("contract-crates", "merge contracts on the same boundary instead of adding one (ADR-0091 D2)"),
    Rule("optional-role", "give the component the role as a typed constructor parameter (ADR-0091 D5)"),
    Rule("bundle-trait", "compose the roles in the composition root, not in a contract (ADR-0091 D4)"),
    Rule("unimplemented-trait", "implement the trait in production or delete it (ADR-0091 D4)"),
)

_TRAIT = re.compile(r"\bpub\s+trait\s+([A-Z]\w*)[^{;]*\{")
_BLOCK = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(trait|impl)\b[^{;]*\{", re.M)
_OPTIONAL_ROLE = re.compile(
    r"\bfn\s+(\w+)[^;{]*?->\s*Option\s*<\s*(?:&\s*(?:'\w+\s+)?(?:mut\s+)?dyn\b|(?:(?:std|alloc)::sync::)?Arc\s*<\s*dyn\b)"
)
_IMPL = re.compile(r"\bimpl\b")


def target_map_errors(contracts: set[str], target_map=TARGET_MAP) -> list[str]:
    merging = {crate: target for target, (_boundary, pending) in target_map.items() for crate in pending}
    errors = [
        f"target-map: {crate} is a contract crate that is not on the target map; a contract crate is one boundary "
        f"(ADR-0091 D2): add the code to the target contract of its boundary, or record the new boundary in "
        f"TARGET_MAP of {Path(__file__).name} with its plan section"
        for crate in sorted(contracts - set(target_map) - set(merging))
    ]
    errors += [
        f"target-map: {crate} merged into {merging[crate]} and is no longer a crate; remove it from TARGET_MAP "
        "in this commit"
        for crate in sorted(set(merging) - contracts)
    ]
    return errors


def _block_end(syntax: str, opening: int) -> int:
    return _crate_boundary_workspace._balanced_end(syntax, opening, "{", "}") or len(syntax)


def _blocks(syntax: str) -> list[tuple[int, int, str, str]]:
    """(open, close, kind, owner) of every trait and impl block; kind is trait, impl-for or inherent."""
    blocks = []
    for match in _BLOCK.finditer(syntax):
        opening = syntax.index("{", match.start())
        head = syntax[match.start():opening]
        if match.group(1) == "trait":
            kind, owner = "trait", re.search(r"\btrait\s+([A-Z]\w*)", head)
        elif re.search(r"\bfor\b", head):
            kind, owner = "impl-for", None
        else:
            kind, owner = "inherent", re.search(r"([A-Z]\w*)\s*(?:<[^{]*)?(?:where\b[^{]*)?$", head)
        blocks.append((opening, _block_end(syntax, opening), kind, owner.group(1) if owner else "?"))
    return blocks


def contract_traits(syntax: str) -> list[tuple[str, int, bool]]:
    """(name, offset, declares a method) of every `pub trait` in one file."""
    found = []
    for match in _TRAIT.finditer(syntax):
        opening = match.end() - 1
        found.append((match.group(1), match.start(), bool(re.search(r"\bfn\s+\w+", syntax[opening:_block_end(syntax, opening)]))))
    return found


def optional_roles(syntax: str) -> list[tuple[str, int]]:
    """(`Owner::method`, offset) of every trait or inherent method returning an optional `dyn` role."""
    blocks = _blocks(syntax)
    found = []
    for match in _OPTIONAL_ROLE.finditer(syntax):
        enclosing = [block for block in blocks if block[0] < match.start() < block[1]]
        if enclosing and enclosing[-1][2] != "impl-for":
            found.append((f"{enclosing[-1][3]}::{match.group(1)}", match.start()))
    return found


def implemented_traits(syntax: str) -> set[str]:
    """Last path segment of the trait of every `impl ... Trait<..> for ...` in one file."""
    names = set()
    syntax = syntax.replace("->", "  ")  # an `Fn() -> T` bound must not close a generic list
    for match in _IMPL.finditer(syntax):
        index = match.end()
        while index < len(syntax) and syntax[index].isspace():
            index += 1
        if syntax.startswith("<", index):
            index = _crate_boundary_workspace._balanced_end(syntax, index, "<", ">") or index
        head = re.match(r"\s*!?\s*((?:\w+\s*::\s*)*)([A-Z]\w*)\s*(<|for\b)", syntax[index:index + 400])
        if not head:
            continue
        if head.group(3) == "<":
            rest = index + head.end() - 1
            after = _crate_boundary_workspace._balanced_end(syntax, rest, "<", ">") or rest
            if not re.match(r"\s*for\b", syntax[after:after + 20]):
                continue
        names.add(head.group(2))
    return names


def measure(sources: tuple[_architecture_ratchet.CrateSource, ...], root: Path) -> tuple[Counts, Places, set[str]]:
    counts: Counts = {rule.name: Counter() for rule in RULES}
    places: Places = {rule.name: defaultdict(list) for rule in RULES}
    contracts = {crate.name for crate in sources if crate.metadata.get("layer") == "contract"}
    counts["contract-crates"]["contract crates"] = len(contracts)
    implemented: set[str] = set()
    declared: dict[str, list[str]] = defaultdict(list)
    for crate in sources:
        for path, syntax in crate.syntax.items():
            relative = path.relative_to(root) if root in path.parents else path
            if crate.metadata.get("layer") != "tooling":
                implemented |= implemented_traits(syntax)
            if crate.name not in contracts:
                continue
            for name, offset, has_method in contract_traits(syntax):
                where = f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}"
                declared[name].append(where)
                if not has_method:
                    counts["bundle-trait"][name] += 1
                    places["bundle-trait"][name].append(where)
            for key, offset in optional_roles(syntax):
                counts["optional-role"][key] += 1
                places["optional-role"][key].append(f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
    for name, where in declared.items():
        if name not in implemented:
            counts["unimplemented-trait"][name] += len(where)
            places["unimplemented-trait"][name] += where
    return ({rule: dict(found) for rule, found in counts.items()},
            {rule: dict(found) for rule, found in places.items()}, contracts)


def _measure_workspace(crates: Path = _crate_boundary_workspace.CRATES) -> tuple[Counts, Places]:
    counts, places, _contracts = measure(_architecture_ratchet.crate_sources(crates), _crate_boundary_workspace.REPO_ROOT)
    return counts, places


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    counts, places, contracts = measure(_architecture_ratchet.crate_sources(crates), _crate_boundary_workspace.REPO_ROOT)
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    errors = target_map_errors(contracts)
    errors += _architecture_ratchet.compare(RULES, counts, places, baseline, baseline_path.name)
    return [f"contract-map: {error}" for error in errors]


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                 | effect |
    | T1   | contract crate is a target or merges into one                         | accept |
    | T2   | contract crate on neither side of the map                             | error |
    | T3   | crate listed as merging no longer exists                              | error: remove it |
    | T4   | the map holds 26 targets, `awaken-session-contract` its only new name, at most 12 pending merges | holds |
    | O1   | trait method / inherent method returns `Option<&dyn R>` / `Option<Arc<dyn R>>` | counted as Owner::method |
    | O2   | the same in an `impl Trait for T` block; `Option<&T>`; `Option<Box<dyn R>>` | not counted |
    | B1   | `pub trait` without a method (bundle, marker, associated items only)  | counted |
    | B2   | `pub trait` with a method; a private trait                            | not counted |
    | U1   | contract trait implemented (plain, generic, blanket, path, generic args, `!`) | not counted |
    | U2   | contract trait only named in a bound or a `dyn`, or implemented only in a tooling crate | counted |
    """
    pending = {"t": ("b", ("old",))}
    assert target_map_errors({"t", "old"}, pending) == [], "T1"
    assert "not on the target map" in target_map_errors({"t", "old", "new"}, pending)[0], "T2"
    assert "remove it from TARGET_MAP" in target_map_errors({"t"}, pending)[0], "T3"
    merging = [crate for _boundary, merged in TARGET_MAP.values() for crate in merged]
    # Plan 9.14 listed 12 pending merges; each landed merge removes its entry.
    assert len(TARGET_MAP) == 26 and len(merging) == len(set(merging)) <= 12, "T4"
    code = (
        "pub trait Session: Send {\n"
        "    fn realization(&self) -> Option<&dyn Realization> { None }\n"
        "    fn shared(&self) -> Option<Arc<dyn Budget>>;\n"
        "    fn plain(&self) -> Option<&Value>;\n"
        "    fn boxed(&self) -> Option<Box<dyn Budget>>;\n"
        "}\n"
        "impl Session for Neutral {\n    fn realization(&self) -> Option<&dyn Realization> { None }\n}\n"
        "impl<T> Registry<T> {\n    pub fn lookup<'a>(&'a self) -> Option<&'a dyn Tool> { None }\n}\n"
        "pub trait Bundle: Session + Budget {}\npub trait Marker {}\npub trait Scope { type Id; }\n"
        "trait Private {}\n"
    )
    syntax = _crate_boundary_workspace.rust_syntax(code)
    assert [key for key, _ in optional_roles(syntax)] == ["Session::realization", "Session::shared",
                                                          "Registry::lookup"], f"O1/O2 {optional_roles(syntax)}"
    assert [(name, has) for name, _, has in contract_traits(syntax)] == [
        ("Session", True), ("Bundle", False), ("Marker", False), ("Scope", False)], "B1/B2"
    impls = implemented_traits(
        "impl Alpha for X {}\nimpl<T: Bound> Beta for T {}\nimpl a::b::Gamma<u8> for Y {}\n"
        "unsafe impl<'a, T, F: Fn() -> u8> Delta<'a, T> for Z<T> where T: Bound {}\nimpl !Epsilon for X {}\n"
        "impl Inherent {}\nimpl<T> Wrapper<T> {}\nfn f(x: &dyn Zeta) {}\n"
    )
    assert impls == {"Alpha", "Beta", "Gamma", "Delta", "Epsilon"}, f"U1/U2 {impls}"

    contract = _architecture_ratchet.CrateSource(
        "c", {"layer": "contract"}, Path("/w/c"),
        {Path("/w/c/src/lib.rs"): "pub trait Used { fn a(&self); }\npub trait Unused { fn b(&self); }\n"
                                  "pub trait OnlyTooling { fn c(&self); }\npub trait Bundle: Used {}\n"},
    )
    user = _architecture_ratchet.CrateSource("u", {"layer": "infrastructure"}, Path("/w/u"),
                                             {Path("/w/u/src/lib.rs"): "impl c::Used for X {}\nimpl<T: Used> Bundle for T {}\n"})
    tooling = _architecture_ratchet.CrateSource("d", {"layer": "tooling"}, Path("/w/d"),
                                                {Path("/w/d/src/lib.rs"): "impl OnlyTooling for X {}\n"})
    counts, places, contracts = measure((contract, user, tooling), Path("/w"))
    assert contracts == {"c"} and counts["contract-crates"] == {"contract crates": 1}, "measure crates"
    assert counts["unimplemented-trait"] == {"Unused": 1, "OnlyTooling": 1}, f"U1/U2 measure {counts}"
    assert counts["bundle-trait"] == {"Bundle": 1} and places["bundle-trait"]["Bundle"] == ["c/src/lib.rs:4"], "B1"
    print("_contract_map_fitness selftest: the decision tables hold")


if __name__ == "__main__":
    raise SystemExit(_architecture_ratchet.main(
        sys.argv[1:], prog=Path(__file__).name, description=__doc__, rules=RULES, measure=_measure_workspace,
        baseline_path=BASELINE, self_test=selftest,
        hard_rules=lambda: target_map_errors({crate.name for crate in _architecture_ratchet.crate_sources(
            _crate_boundary_workspace.CRATES) if crate.metadata.get("layer") == "contract"})))
