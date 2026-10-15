"""Contract crates follow the target map and contract traits are admitted by their callers.

ADR-0091 D2, D4, D5 (plan 9.14, W28). Counted from production code (comments,
string literals and `cfg(test)` code are not scanned):

* `target-map` (a hard rule, not a ratchet): every crate of layer `contract` is
  a target contract of `TARGET_MAP` or is listed there as merging into one. A
  crate listed as merging that no longer exists is an error too, so the list of
  pending merges only shrinks. The map encodes plan 9.14: 27 target contracts,
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
* `foreign-persistence-port`: a `pub trait` of a contract crate named `*Repository`
  or `*Store` (a persistence port) that the production code of another bounded
  context names, keyed `Trait <- crate`. Persistence is private to its context
  (plan 9.19 item 1): a crate may name the port only when it shares the declaring
  crate's `bounded_context`, is a composition root (layer `bootstrap`, or no
  `bounded_context`), is of layer `tooling`, or is a storage backend -- a crate of
  layer `infrastructure` whose name contains `store` and that implements the
  trait (`impl ... Trait for`); a backend inside the same context is already
  allowed by the first clause. Ports of the `SDK release` contracts of
  `TARGET_MAP` (and the contracts merging into them) are published for platform
  use and are not counted. A name declared in several contract crates is allowed
  when any declaring crate allows it. "Names" is a whole-word match in production
  code, so a `use`, a bound or a `dyn` all count, once per crate.
* `fat-trait`: a `pub trait` of a contract crate declaring more than
  `FAT_TRAIT_METHODS` (12) methods (each `fn` directly in the trait body, so a
  `fn` nested in a default body is not one), keyed by trait name with its method
  count. One trait serves one caller role (D4): split it by caller. Besides the
  ratchet this is an admission rule (a hard error that `--reason` cannot lift):
  a trait the baseline does not record may not exceed 12 methods, and a recorded
  count may only fall -- a count recorded through `--reason` is an error too.
* `duplicate-type`: a name declared `pub struct`, `pub enum`, `pub trait` or
  `pub type` (exactly `pub`, so `pub(crate)` is not public) in two or more
  contract crates, keyed by name with the number of contract crates declaring it.
  One concept has one definition (plan 9.19 item 4); a name repeated inside one
  crate is not counted.
* `contract-reexport`: a `pub use` (exactly `pub`) in a contract crate whose path
  starts at another first-party crate (a workspace member other than itself),
  keyed by the contract crate with the number of items re-exported (the leaves of
  the use tree; a glob or `self` is one item). A user depends on the owner
  directly (plan 9.19 item 5). Re-exports of the crate's own modules
  (`crate::`, `self::`, `super::`, a local module or enum) and of third-party or
  standard-library crates are not counted: the rule is about one first-party type
  reachable through several contracts.
* `renamed-reexport`: the items of such a re-export given another name with `as`
  (`as _` is not a name), keyed by the contract crate with their number. One type
  has one name (plan 9.19 item 5).
* `aggregate-pub-field`: a public field of a struct declared in a Session
  aggregate-state contract (`AGGREGATE_STATE_CRATES`: the state contract and the
  `awaken-session-contract` it merges into, so the rule follows the code), keyed
  by struct name with its number of fields declared exactly `pub` (a named field
  `pub name: T` or a tuple field `pub T`; `pub(crate)` and private fields are not
  public). Aggregate state changes through its constructors and transition
  methods, and the contract publishes read-only projections (plan 9.19 item 2,
  W26); a struct not recorded may not gain a public field.

Every ratchet is held at or below `contract_map_baseline.json`; see
`_architecture_ratchet.py` for how a fall is recorded
(`python3 scripts/ci/_contract_map_fitness.py --write-baseline`).
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from functools import lru_cache
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
    "awaken-runtime-contract": ("SDK release", ()),
    # Existing separately published Web vocabulary owns the pure policy kernel;
    # merging it into Runtime would reverse the extension ownership boundary.
    "awaken-web-policy-contract": ("SDK release", ()),
    "awaken-session-types": ("Session", ()),
    "awaken-session-contract": ("Session", (
        "awaken-session-state-contract", "awaken-session-event-contract", "awaken-session-budget-contract",
        "awaken-session-dream-contract", "awaken-session-repository-contract",
    )),
    "awaken-session-configuration-contract": ("Session", ()),
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

# Contracts published with the SDK (and those merging into them): their ports serve the platform too.
SDK_RELEASE_CONTRACTS = frozenset(
    crate for target, (boundary, pending) in TARGET_MAP.items() if boundary == "SDK release"
    for crate in (target, *pending)
)
PERSISTENCE_PORT = re.compile(r"(?:Repository|Store)$")
COMPOSITION_LAYER = "bootstrap"
TOOLING_LAYER = "tooling"
STORAGE_LAYER = "infrastructure"
STORAGE_NAME = "store"
FAT_TRAIT_METHODS = 12
# Session aggregate state: the state contract and the merge target of TARGET_MAP that absorbs it.
AGGREGATE_STATE_CRATES = frozenset({"awaken-session-state-contract", "awaken-session-contract"})

RULES = (
    Rule("contract-crates", "merge contracts on the same boundary instead of adding one (ADR-0091 D2)"),
    Rule("optional-role", "give the component the role as a typed constructor parameter (ADR-0091 D5)"),
    Rule("bundle-trait", "compose the roles in the composition root, not in a contract (ADR-0091 D4)"),
    Rule("unimplemented-trait", "implement the trait in production or delete it (ADR-0091 D4)"),
    Rule("foreign-persistence-port", "use the owning context's application port or a narrow observation; "
         "persistence is private to its context (ADR-0091 D2, plan 9.19 item 1)"),
    Rule("fat-trait", "split the trait into one role trait per caller; the implementation implements every role "
         "(ADR-0091 D4, plan 9.19 item 3)"),
    Rule("duplicate-type", "keep one definition of the concept, or name each by its own meaning "
         "(ADR-0091 D2, plan 9.19 item 4)"),
    Rule("contract-reexport", "depend on the owning crate directly instead of re-exporting its items; SDK "
         "convenience paths live only in the SDK facade (ADR-0091 D2, plan 9.19 item 5)"),
    Rule("renamed-reexport", "use the owner's name; a renamed re-export gives one type two names "
         "(ADR-0091 D2, plan 9.19 item 5)"),
    Rule("aggregate-pub-field", "make the field private and change it through a constructor or transition "
         "method; publish a read-only projection (plan 9.19 item 2, W26)"),
)

_TRAIT = re.compile(r"\bpub\s+trait\s+([A-Z]\w*)[^{;]*\{")
_BLOCK = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(trait|impl)\b[^{;]*\{", re.M)
_OPTIONAL_ROLE = re.compile(
    r"\bfn\s+(\w+)[^;{]*?->\s*Option\s*<\s*(?:&\s*(?:'\w+\s+)?(?:mut\s+)?dyn\b|(?:(?:std|alloc)::sync::)?Arc\s*<\s*dyn\b)"
)
_IMPL = re.compile(r"\bimpl\b")
_BODY_TOKEN = re.compile(r"[{}]|\bfn\s+\w+")
_PUB_USE = re.compile(r"\bpub\s+use\s+([^;]*);")
_STRUCT = re.compile(r"\bstruct\s+([A-Z]\w*)")
_ATTRIBUTES = re.compile(r"^(?:\s*#\s*\[[^\]]*\])*\s*")
_PUBLIC_NAMED_FIELD = re.compile(r"pub\s+(?:r#)?\w+\s*:")
_PUBLIC_TUPLE_FIELD = re.compile(r"pub\b(?!\s*\()")
_PUBLIC_TYPE = re.compile(r"\bpub\s+(?:unsafe\s+)?(?:struct|enum|trait|type)\s+([A-Za-z_]\w*)")


def target_map_errors(contracts: set[str], target_map=TARGET_MAP) -> list[str]:
    merging = {crate: target for target, (_boundary, pending) in target_map.items() for crate in pending}
    errors = [
        _architecture_ratchet.diagnostic(
            "target-map", crate, "listed boundary", "unlisted contract",
            f"add the code to the target contract of its boundary, or record the new boundary in "
            f"{Path(__file__).name}::TARGET_MAP with its plan section (ADR-0091 D2)")
        for crate in sorted(contracts - set(target_map) - set(merging))
    ]
    errors += [
        _architecture_ratchet.diagnostic(
            "target-map", crate, "pending merge", "absent crate",
            f"remove its completed merge into {merging[crate]} from {Path(__file__).name}::TARGET_MAP in this commit")
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


def trait_methods(body: str) -> int:
    """Methods declared directly in a trait body (the text between its braces)."""
    depth = methods = 0
    for token in _BODY_TOKEN.finditer(body):
        if token.group(0) == "{":
            depth += 1
        elif token.group(0) == "}":
            depth -= 1
        elif depth == 0:
            methods += 1
    return methods


def contract_traits(syntax: str) -> list[tuple[str, int, int]]:
    """(name, offset, declared methods) of every `pub trait` in one file."""
    found = []
    for match in _TRAIT.finditer(syntax):
        opening = match.end() - 1
        found.append((match.group(1), match.start(), trait_methods(syntax[opening + 1:_block_end(syntax, opening)])))
    return found


def public_types(syntax: str) -> list[tuple[str, int]]:
    """(name, offset) of every `pub struct|enum|trait|type` declaration in one file."""
    return [(match.group(1), match.start()) for match in _PUBLIC_TYPE.finditer(syntax)]


def use_leaves(tree: str) -> list[str]:
    """The flattened paths of one use tree, each with its `as` alias: `a::{b::C, D as E}` -> `a::b::C`, `a::D as E`."""
    tree = re.sub(r"\s*(::|[{},])\s*", r"\1", " ".join(tree.split())).strip()
    opening = tree.find("{")
    if opening < 0:
        return [tree] if tree else []
    inner = tree[opening + 1:(_crate_boundary_workspace._balanced_end(tree, opening, "{", "}") or len(tree)) - 1]
    parts, depth, start = [], 0, 0
    for cursor, character in enumerate(inner):
        depth += {"{": 1, "}": -1}.get(character, 0)
        if character == "," and depth == 0:
            parts.append(inner[start:cursor])
            start = cursor + 1
    parts.append(inner[start:])
    return [tree[:opening] + leaf for part in parts for leaf in use_leaves(part)]


def renamed(leaf: str) -> bool:
    """Whether a use-tree leaf gives its item another name (`as _` only imports it anonymously)."""
    alias = re.search(r"\sas\s+(\w+)$", leaf)
    return bool(alias) and alias.group(1) != "_"


def foreign_reexports(syntax: str, foreign: set[str]) -> list[tuple[int, list[str]]]:
    """(offset, leaves) of every `pub use` whose leaves start at a crate of `foreign` (underscored names)."""
    found = []
    for match in _PUB_USE.finditer(syntax):
        leaves = [leaf for leaf in use_leaves(match.group(1))
                  if re.split(r"\s+as\s+", leaf)[0].lstrip(":").split("::")[0] in foreign]
        if leaves:
            found.append((match.start(), leaves))
    return found


def _top_level_fields(body: str) -> list[str]:
    fields, depth, start = [], 0, 0
    for cursor, character in enumerate(body):
        depth += {"(": 1, "[": 1, "{": 1, "<": 1, ")": -1, "]": -1, "}": -1, ">": -1}.get(character, 0)
        if character == "," and depth == 0:
            fields.append(body[start:cursor])
            start = cursor + 1
    fields.append(body[start:])
    return [_ATTRIBUTES.sub("", field, count=1) for field in fields if field.strip()]


def public_fields(syntax: str) -> list[tuple[str, int, int]]:
    """(struct, offset, fields declared exactly `pub`) of every struct with a public field in one file."""
    syntax = syntax.replace("->", "  ")  # an `Fn() -> T` bound must not close a generic list
    found = []
    for match in _STRUCT.finditer(syntax):
        index = match.end()
        while index < len(syntax) and syntax[index].isspace():
            index += 1
        if syntax.startswith("<", index):
            index = _crate_boundary_workspace._balanced_end(syntax, index, "<", ">") or index
        tuple_body = re.match(r"\s*\(", syntax[index:])
        if tuple_body:
            opening = index + tuple_body.end() - 1
            close = _crate_boundary_workspace._balanced_end(syntax, opening, "(", ")") or len(syntax)
            fields = [field for field in _top_level_fields(syntax[opening + 1:close - 1])
                      if _PUBLIC_TUPLE_FIELD.match(field)]
        else:
            body = re.compile(r"[{;]").search(syntax, index)
            if not body or body.group(0) == ";":
                continue
            close = _block_end(syntax, body.start())
            fields = [field for field in _top_level_fields(syntax[body.start() + 1:close - 1])
                      if _PUBLIC_NAMED_FIELD.match(field)]
        if fields:
            found.append((match.group(1), match.start(), len(fields)))
    return found


def fat_trait_errors(counts: Counts, baseline: dict) -> list[str]:
    """The admission half of `fat-trait`: no new fat trait, and no recorded growth."""
    recorded = baseline.get("fat-trait", {})
    grown = baseline.get("growth_reasons", {}).get("fat-trait", {})
    return [
        _architecture_ratchet.diagnostic(
            "fat-trait", name, recorded.get(name, FAT_TRAIT_METHODS), methods,
            "split it by caller (ADR-0091 D4); --reason cannot admit it",
            details="growth reason recorded" if name in grown else "unrecorded" if name not in recorded else "recorded count grew")
        for name, methods in sorted(counts.get("fat-trait", {}).items())
        if name not in recorded or name in grown or methods > recorded[name]
    ]


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


def persistence_port_allowed(port_crate: _architecture_ratchet.CrateSource,
                             user: _architecture_ratchet.CrateSource, implements: bool) -> bool:
    """Whether `user` may name a persistence port declared in contract crate `port_crate`."""
    layer, context = user.metadata.get("layer"), user.metadata.get("bounded_context")
    return (port_crate.name in SDK_RELEASE_CONTRACTS or layer in (COMPOSITION_LAYER, TOOLING_LAYER)
            or context is None or context == port_crate.metadata.get("bounded_context")
            or (implements and layer == STORAGE_LAYER and STORAGE_NAME in user.name))


def _foreign_persistence_ports(sources, ports: dict[str, list], implemented_by: dict[str, set[str]],
                               root: Path, counts: Counts, places: Places) -> None:
    if not ports:
        return
    named = re.compile(r"\b(" + "|".join(sorted(map(re.escape, ports))) + r")\b")
    for crate in sources:
        first: dict[str, str] = {}
        for path, syntax in crate.syntax.items():
            for match in named.finditer(syntax):
                relative = (path.relative_to(root) if root in path.parents else path).as_posix()
                first.setdefault(match.group(1), f"{relative}:{_architecture_ratchet.line_of(syntax, match.start())}")
        for name, where in sorted(first.items()):
            implements = name in implemented_by.get(crate.name, set())
            if not any(persistence_port_allowed(owner, crate, implements) for owner in ports[name]):
                counts["foreign-persistence-port"][f"{name} <- {crate.name}"] = 1
                places["foreign-persistence-port"][f"{name} <- {crate.name}"] = [where]


def measure(sources: tuple[_architecture_ratchet.CrateSource, ...], root: Path) -> tuple[Counts, Places, set[str]]:
    counts: Counts = {rule.name: Counter() for rule in RULES}
    places: Places = {rule.name: defaultdict(list) for rule in RULES}
    contracts = {crate.name for crate in sources if crate.metadata.get("layer") == "contract"}
    first_party = {crate.name.replace("-", "_") for crate in sources}
    counts["contract-crates"]["contract crates"] = len(contracts)
    implemented: set[str] = set()
    implemented_by: dict[str, set[str]] = defaultdict(set)
    declared: dict[str, list[str]] = defaultdict(list)
    ports: dict[str, list[_architecture_ratchet.CrateSource]] = defaultdict(list)
    typed: dict[str, dict[str, list[str]]] = defaultdict(lambda: defaultdict(list))  # name -> crate -> places
    for crate in sources:
        for path, syntax in crate.syntax.items():
            relative = (path.relative_to(root) if root in path.parents else path).as_posix()
            implemented_by[crate.name] |= implemented_traits(syntax)
            if crate.metadata.get("layer") != TOOLING_LAYER:
                implemented |= implemented_by[crate.name]
            if crate.name in AGGREGATE_STATE_CRATES:
                for name, offset, fields in public_fields(syntax):
                    counts["aggregate-pub-field"][name] += fields
                    places["aggregate-pub-field"][name].append(
                        f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
            if crate.name not in contracts:
                continue
            for name, offset, methods in contract_traits(syntax):
                where = f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}"
                declared[name].append(where)
                if methods > FAT_TRAIT_METHODS:
                    counts["fat-trait"][name] += methods
                    places["fat-trait"][name].append(where)
                if PERSISTENCE_PORT.search(name) and crate not in ports[name]:
                    ports[name].append(crate)
                if not methods:
                    counts["bundle-trait"][name] += 1
                    places["bundle-trait"][name].append(where)
            for offset, leaves in foreign_reexports(syntax, first_party - {crate.name.replace("-", "_")}):
                where = f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}"
                counts["contract-reexport"][crate.name] += len(leaves)
                places["contract-reexport"][crate.name].append(where)
                if any(map(renamed, leaves)):
                    counts["renamed-reexport"][crate.name] += sum(map(renamed, leaves))
                    places["renamed-reexport"][crate.name].append(where)
            for name, offset in public_types(syntax):
                typed[name][crate.name].append(f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
            for key, offset in optional_roles(syntax):
                counts["optional-role"][key] += 1
                places["optional-role"][key].append(f"{relative}:{_architecture_ratchet.line_of(syntax, offset)}")
    _foreign_persistence_ports(sources, ports, implemented_by, root, counts, places)
    for name, crates in typed.items():
        if len(crates) > 1:
            counts["duplicate-type"][name] = len(crates)
            places["duplicate-type"][name] = [where for found in crates.values() for where in found]
    for name, where in declared.items():
        if name not in implemented:
            counts["unimplemented-trait"][name] += len(where)
            places["unimplemented-trait"][name] += where
    return ({rule: dict(found) for rule, found in counts.items()},
            {rule: dict(found) for rule, found in places.items()}, contracts)


@lru_cache(maxsize=2)
def _workspace(crates: Path = _crate_boundary_workspace.CRATES) -> tuple[Counts, Places, set[str]]:
    return measure(_architecture_ratchet.crate_sources(crates), _crate_boundary_workspace.REPO_ROOT)


def _measure_workspace() -> tuple[Counts, Places]:
    counts, places, _contracts = _workspace()
    return counts, places


def hard_errors(counts: Counts, contracts: set[str], baseline: dict) -> list[str]:
    return target_map_errors(contracts) + fat_trait_errors(counts, baseline)


def _workspace_hard_errors(previous: dict) -> list[str]:
    counts, _places, contracts = _workspace()
    return hard_errors(counts, contracts, previous)


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    counts, places, contracts = _workspace(crates)
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    errors = hard_errors(counts, contracts, baseline)
    errors += _architecture_ratchet.compare(
        RULES, counts, places, baseline, baseline_path.name,
        lower_advice=f"python3 scripts/ci/{Path(__file__).name} --tighten to lower {baseline_path.name} in this commit")
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
    | P1   | another context names a `*Repository` / `*Store` contract trait (use, bound or `dyn`), once per crate | counted `Trait <- crate` |
    | P2   | the user shares the declaring crate's bounded context                 | not counted |
    | P3   | composition root (layer `bootstrap`, or no bounded context); tooling crate | not counted |
    | P4   | infrastructure crate named `*store*` implementing the port (storage backend) | not counted |
    | P5   | implements the port but is not a `*store*` infrastructure crate; names a `*store*` port without implementing it | counted |
    | P6   | port declared in an `SDK release` contract; name not ending in Repository/Store; named only in a comment or literal | not counted |
    | F1   | contract `pub trait` with 13 methods                                   | counted with value 13 |
    | F2   | 12 methods (a `fn` nested in a default body, a `fn(..)` type, a private trait do not count) | not counted |
    | F3   | fat trait the baseline records at its count                            | accept |
    | F4   | fat trait the baseline does not record, or recorded through --reason   | hard error naming --reason |
    | D1   | `pub` struct / enum / trait / type alias of one name in two contract crates | counted, value = crates |
    | D2   | the name twice in one crate; `pub(crate)` or private in the other; declared in a non-contract crate | not counted |
    | R1   | `pub use` of another first-party crate's items (nested groups, glob, `self`, the whole crate, `::` prefix) | counted per contract crate, value = leaves |
    | R2   | own module paths (`crate::`, `self::`, `super::`, local module or enum), own crate name | not counted |
    | R3   | third-party or std crate; `pub(crate) use` / `use`; non-contract crate; comment, literal or `cfg(test)` | not counted |
    | N1   | an item of a counted re-export renamed with `as` (also the whole crate) | `renamed-reexport` per contract crate, value = renamed items |
    | N2   | `as _`; a renamed re-export of the crate's own or a third-party item    | not counted |
    | A1   | aggregate-state struct with `pub` named fields (attributes, generics, `where`, nested commas) | counted per struct, value = public fields |
    | A2   | private field; `pub(crate)` field; unit struct; enum struct variant   | not counted |
    | A3   | tuple struct `pub` field                                               | counted; `pub(crate)` tuple field not |
    | A4   | struct in `cfg(test)` code; struct in a crate outside AGGREGATE_STATE_CRATES | not counted |
    | A5   | ratchet: a public field added (recorded or new struct) / one removed   | error: grown / error: lower the baseline |
    | A6   | AGGREGATE_STATE_CRATES are on TARGET_MAP (a target or merging into one) | holds |
    """
    pending = {"t": ("b", ("old",))}
    assert target_map_errors({"t", "old"}, pending) == [], "T1"
    assert "listed boundary -> unlisted contract" in target_map_errors({"t", "old", "new"}, pending)[0], "T2"
    assert "pending merge -> absent crate" in target_map_errors({"t"}, pending)[0], "T3"
    merging = [crate for _boundary, merged in TARGET_MAP.values() for crate in merged]
    # Plan 9.14 listed 12 pending merges. The 2026-10-15 amendment retains
    # the existing pure Web SDK as its own target without creating a crate.
    assert len(TARGET_MAP) == 27 and len(merging) == len(set(merging)) <= 11, "T4"
    assert TARGET_MAP["awaken-web-policy-contract"] == ("SDK release", ()), "T4 Web owner"
    assert TARGET_MAP["awaken-runtime-contract"] == ("SDK release", ()), "T4 neutral Runtime"
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
    assert [(name, methods) for name, _, methods in contract_traits(syntax)] == [
        ("Session", 4), ("Bundle", 0), ("Marker", 0), ("Scope", 0)], "B1/B2"
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
    _selftest_persistence_ports()
    _selftest_fat_traits()
    _selftest_duplicate_types()
    _selftest_reexports()
    _selftest_aggregate_fields()
    _selftest_baseline_snapshot()
    print("_contract_map_fitness selftest: the decision tables hold")


def _crate(name: str, layer: str, context: str | None, code: str) -> _architecture_ratchet.CrateSource:
    metadata = {"layer": layer} | ({"bounded_context": context} if context else {})
    return _architecture_ratchet.CrateSource(name, metadata, Path(f"/w/{name}"), {Path(f"/w/{name}/src/lib.rs"): code})


def _selftest_fat_traits() -> None:
    # D7: unheld trait / existing growth reason -> original hard admission
    # failure, independently of its numeric relation. Explicit metadata state
    # and the caller-role fix must never suggest --reason can authorize it.
    methods = "".join(f"    fn m{index}(&self);\n" for index in range(11))
    nested = "    fn last(&self) -> u8 { fn helper() -> u8 { 1 } let f: fn(u8) = drop; helper() }\n"
    code = (f"pub trait Wide {{\n{methods}{nested}    fn extra(&self);\n}}\n"
            f"pub trait Twelve {{\n{methods}{nested}}}\ntrait Hidden {{\n{methods}{nested}    fn extra(&self);\n}}\n")
    sources = (_crate("c", "contract", "c", code), _crate("u", "infrastructure", "c",
                                                          "impl Wide for X {}\nimpl Twelve for X {}\n"))
    counts, places, contracts = measure(sources, Path("/w"))
    assert counts["fat-trait"] == {"Wide": 13} and places["fat-trait"]["Wide"] == ["c/src/lib.rs:1"], f"F1/F2 {counts}"
    assert fat_trait_errors(counts, {"fat-trait": {"Wide": 13}}) == [] and contracts == {"c"}, "F3"
    unrecorded = fat_trait_errors(counts, {"fat-trait": {}})
    assert len(unrecorded) == 1 and "fat-trait: Wide 12 -> 13" in unrecorded[0] and "unrecorded" in unrecorded[0] \
        and "--reason" in unrecorded[0], "F4"
    reasoned = fat_trait_errors(counts, {"fat-trait": {"Wide": 13}, "growth_reasons": {"fat-trait": {"Wide": "x"}}})
    assert len(reasoned) == 1 and "growth reason recorded" in reasoned[0], "F4 reason"
    assert "--reason cannot admit it" in reasoned[0], "D7 hard admission has no reasoned bypass"
    # D10 admission table: unrecorded OR prior growth-reason key OR incoming
    # count above held -> hard error. Equal/fall keep the original admission;
    # numeric fall recording remains the shared CLI's separate decision.
    for held, reasons, expected in (({}, {}, (True, True, True)),
                                   ({"Wide": 14}, {}, (False, False, True)),
                                   ({"Wide": 14}, {"Wide": "reviewed"}, (True, True, True)),
                                   ({"Wide": 14}, {"Wide": ""}, (True, True, True))):
        for methods, failed in zip((13, 14, 15), expected):
            previous = {"fat-trait": held, "growth_reasons": {"fat-trait": reasons}}
            errors = fat_trait_errors({"fat-trait": {"Wide": methods}}, previous)
            assert bool(errors) == failed, "D10 original admission plus incoming growth"
            if errors:
                assert len(errors) == 1 and "--reason cannot admit it" in errors[0], "D10 intersecting hard causes stay one finding"


def _selftest_duplicate_types() -> None:
    sources = (
        _crate("a", "contract", "x", "pub struct Lease;\npub enum Error {}\npub type Alias = u8;\n"
                                     "pub trait Port { fn p(&self); }\npub struct Twice;\nmod m { pub struct Twice; }\n"
                                     "pub(crate) struct Inner;\nstruct Private;\n"),
        _crate("b", "contract", "y", "pub struct Lease { id: u8 }\npub struct Error;\npub(crate) type Alias = u8;\n"
                                     "pub unsafe trait Port {}\npub struct Inner;\npub struct Private;\n"),
        _crate("c", "contract", "z", "pub type Lease<T> = Vec<T>;\n"),
        _crate("d", "application", "x", "pub struct Twice;\npub struct Alias;\n"),
    )
    counts, places, _contracts = measure(sources, Path("/w"))
    assert counts["duplicate-type"] == {"Lease": 3, "Error": 2, "Port": 2}, f"D1/D2 {counts['duplicate-type']}"
    assert places["duplicate-type"]["Error"] == ["a/src/lib.rs:2", "b/src/lib.rs:2"], "D1 where"


def _selftest_reexports() -> None:
    assert use_leaves("a::{b::{C, D as E},\n  F, self, *}") == ["a::b::C", "a::b::D as E", "a::F", "a::self", "a::*"], \
        f"R1 leaves {use_leaves('a::{b::{C, D as E}, F, self, *}')}"
    code = (
        "pub use other::{a::B, c::{D, E as F}, self};\npub use ::other::g::*;\npub use other as alias;\n"
        "pub use crate::x::Y;\npub use self::m::Z;\npub use super::W;\npub use local::V;\npub use Kind::*;\n"
        "pub use other::m::{N as _, O as Q};\npub use crate::own::P as R;\npub use serde::Serialize as Ser;\n"
        "pub use c::Own;\npub use serde::Serialize;\npub use std::sync::Arc;\npub(crate) use other::H;\n"
        "use other::I;\n// pub use other::J;\nconst S: &str = \"pub use other::K;\";\n"
        "#[cfg(test)]\nmod tests { pub use other::L; }\n"
    )
    sources = tuple(_architecture_ratchet.CrateSource(
        crate.name, crate.metadata, crate.directory,
        {path: _crate_boundary_workspace.production_rust(text) for path, text in crate.files.items()})
        for crate in (_crate("c", "contract", "x", code), _crate("other", "contract", "y", ""),
                      _crate("app", "application", "x", "pub use other::B;\n")))
    counts, places, _contracts = measure(sources, Path("/w"))
    assert counts["contract-reexport"] == {"c": 8}, f"R1-R3 {counts['contract-reexport']}"
    assert places["contract-reexport"]["c"] == ["c/src/lib.rs:1", "c/src/lib.rs:2", "c/src/lib.rs:3",
                                                "c/src/lib.rs:9"], "R1 where"
    assert counts["renamed-reexport"] == {"c": 3}, f"N1/N2 {counts['renamed-reexport']}"
    assert places["renamed-reexport"]["c"] == ["c/src/lib.rs:1", "c/src/lib.rs:3", "c/src/lib.rs:9"], "N1 where"


def _selftest_aggregate_fields() -> None:
    code = (
        "pub struct State<T: Clone> where T: Fn() -> u8 {\n"
        "    #[serde(default)]\n    pub id: Id,\n    pub map: HashMap<String, Vec<(u8, u8)>>,\n"
        "    private: u8,\n    pub(crate) inner: u8,\n    pub r#type: T,\n}\n"
        "pub struct Pair(pub u8, pub(crate) u8, u16);\npub struct Unit;\nstruct Hidden { value: u8 }\n"
        "pub enum Phase { Running { pub_like: u8 } }\n"
        "#[cfg(test)]\nmod tests { pub struct Fixture { pub value: u8 } }\n"
    )
    sources = tuple(_architecture_ratchet.CrateSource(
        crate.name, crate.metadata, crate.directory,
        {path: _crate_boundary_workspace.production_rust(text) for path, text in crate.files.items()})
        for crate in (_crate("awaken-session-state-contract", "contract", "session", code),
                      _crate("other", "contract", "session", "pub struct Open { pub value: u8 }\n")))
    counts, places, _contracts = measure(sources, Path("/w"))
    assert counts["aggregate-pub-field"] == {"State": 3, "Pair": 1}, f"A1-A4 {counts['aggregate-pub-field']}"
    assert places["aggregate-pub-field"]["Pair"] == ["awaken-session-state-contract/src/lib.rs:9"], "A3 where"
    rules = tuple(rule for rule in RULES if rule.name == "aggregate-pub-field")
    held = {"aggregate-pub-field": {"State": 3, "Pair": 1}}
    assert _architecture_ratchet.compare(rules, counts, places, held, "b.json") == [], "A5 equal"
    grown = _architecture_ratchet.compare(rules, {"aggregate-pub-field": {"State": 4, "Pair": 1, "New": 1}}, places,
                                          held, "b.json")
    assert len(grown) == 2 and all(" -> " in error and "make the field private" in error for error in grown), \
        f"A5 growth {grown}"
    fell = _architecture_ratchet.compare(rules, {"aggregate-pub-field": {"State": 2}}, places, held, "b.json")
    assert len(fell) == 2 and all("lower b.json" in error for error in fell), f"A5 fall {fell}"
    merging = {crate for _boundary, pending in TARGET_MAP.values() for crate in pending}
    assert AGGREGATE_STATE_CRATES <= set(TARGET_MAP) | merging, "A6"


def _selftest_persistence_ports() -> None:
    sdk = next(iter(SDK_RELEASE_CONTRACTS))
    sources = (
        _crate("port", "contract", "memory", "pub trait MemoryStore { fn a(&self); }\n"
               "pub trait MemoryRepository { fn b(&self); }\npub trait StoreConfig { fn c(&self); }\n"),
        _crate(sdk, "contract", "agent-runtime", "pub trait CheckpointStore { fn d(&self); }\n"),
        _crate("app", "application", "memory", "fn f(s: &dyn MemoryStore) {}\n"),
        _crate("edge", "interface", "protocols",
               "use port::{MemoryStore, StoreConfig};\nfn f<R: MemoryRepository>(s: &dyn MemoryStore) {}\n"
               "fn g(c: &dyn CheckpointStore) {}\n"),
        _crate("root", "bootstrap", "memory-root", "use port::MemoryStore;\n"),
        _crate("bin", "interface", None, "use port::MemoryStore;\n"),
        _crate("kit", "tooling", "testing", "use port::MemoryStore;\n"),
        _crate("memory-pg-store", "infrastructure", "storage", "impl port::MemoryStore for Pg {}\n"
               "fn f(r: &dyn MemoryRepository) {}\n"),
        _crate("host", "infrastructure", "execution", "impl MemoryStore for Host {}\n"),
        _crate("quiet", "interface", "protocols", "// MemoryStore\nconst NAME: &str = \"MemoryRepository\";\n"),
    )
    sources = tuple(_architecture_ratchet.CrateSource(
        crate.name, crate.metadata, crate.directory,
        {path: _crate_boundary_workspace.production_rust(text) for path, text in crate.files.items()})
        for crate in sources)
    counts, places, _contracts = measure(sources, Path("/w"))
    assert counts["foreign-persistence-port"] == {
        "MemoryRepository <- edge": 1, "MemoryStore <- edge": 1, "MemoryRepository <- memory-pg-store": 1,
        "MemoryStore <- host": 1}, f"P1-P6 {counts['foreign-persistence-port']}"
    assert places["foreign-persistence-port"]["MemoryStore <- edge"] == ["edge/src/lib.rs:1"], "P1 where"



def _selftest_baseline_snapshot() -> None:
    """D10 graph: parsed mode -> one original record -> cached measurement.

    Unrecorded / previous growth reason / incoming growth -> hard refusal
    before rendering, independently of reason/first-record mode. A hypothetical
    second record cannot change that authority. Existing equal/fall behavior
    remains with the shared numeric check and writer.

    Rule | cause                                       | effect
    D10a | first hard, hypothetical second admits      | refuse, 1 read/1 measurement, exact bytes
    D10b | first admits, hypothetical second hard      | accept, 1 read/1 measurement, exact bytes
    D10c | unrecorded or held13 ->14; all modes/reason   | hard refusal, no candidate write
    D10d | held13 equal / held14 falls to13            | equal unchanged / original tighten/write lower
    D10e | missing record + explicit write zero / fat  | initialize zero / refuse fat, no reader
    D10f | malformed argv or missing readonly record   | refuse before reads/measurement

    Execute this module's actual __main__ block. The fixture controls source
    measurements, while the real cached _workspace and Path.read_text run;
    this distinguishes duplicate record IO from cached corpus acquisition.
    """
    import ast
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    tree = ast.parse(Path(__file__).read_text(encoding="utf-8"))
    entry = next(node for node in tree.body if isinstance(node, ast.If)
                 and ast.unparse(node.test) == "__name__ == '__main__'")
    entry_code = compile(ast.Module(body=[entry], type_ignores=[]), __file__, "exec")
    held = {"owner": "baseline-owner", "foreign": {"keep": [1, 2]}, "fat-trait": {"Wide": 13}}
    reasoned = {**held, "growth_reasons": {"fat-trait": {"Wide": "previous inadmissible growth"}}}
    contracts = set(TARGET_MAP) | {name for _, pending in TARGET_MAP.values() for name in pending}
    original_read = Path.read_text

    def invoke(argv, previous=held, hypothetical=reasoned, count=13):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "baseline.json"
            if previous is not None:
                path.write_text(json.dumps(previous) + "\n")
            before = path.read_bytes() if path.exists() else None
            reads, measured = [], []
            def read(target, *args, **kwargs):
                if target == path:
                    reads.append(target)
                    return json.dumps(previous if len(reads) == 1 else hypothetical)
                return original_read(target, *args, **kwargs)
            def measure_fixture(_sources, _root):
                measured.append(True)
                return ({"fat-trait": {"Wide": count}} if count else {}), {}, contracts
            output, errors = io.StringIO(), io.StringIO()
            _workspace.cache_clear()
            with patch.dict(globals(), {"__name__": "__main__", "BASELINE": path, "measure": measure_fixture}), \
                    patch.object(_architecture_ratchet, "crate_sources", lambda _crates: ()), \
                    patch.object(Path, "read_text", read), patch.object(sys, "argv", [__file__, *argv]), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    exec(entry_code, globals())
                except SystemExit as error:
                    status = error.code
            _workspace.cache_clear()
            after = path.read_bytes() if path.exists() else None
            assert len(reads) <= 1 and len(measured) <= 1, "D10 one record and cached workspace snapshot"
            return status, before, after, reads, measured, errors.getvalue()

    modes = ([], ["--check"], ["--tighten"], ["--write-baseline"], ["--write-baseline", "--reason", "reviewed"])
    # Prepare a normalized, hard-valid record through the existing writer.
    # Partial fixture records otherwise legitimately gain its empty rule slots
    # and owning comment; that is not a change to the equal-count policy.
    status, _, after, _, _, _ = invoke(["--write-baseline"])
    assert status == 0
    equal = json.loads(after)
    assert equal["owner"] == held["owner"] and equal["foreign"] == held["foreign"]
    for argv in modes:
        for previous, count in ((reasoned, 13), ({**held, "fat-trait": {}}, 13),
                                ({"owner": "baseline-owner"}, 13), (held, 14)):
            status, before, after, reads, measured, errors = invoke(argv, previous, held, count)
            assert status == 1 and before == after and len(reads) == len(measured) == 1 \
                and "--reason cannot admit it" in errors, "D10a/c previous hard authority cannot be replaced by a candidate"
        status, before, after, reads, measured, _ = invoke(argv, equal)
        assert status == 0 and before == after and len(reads) == len(measured) == 1, "D10b/d equal authority, no second record"
    fallen = {**equal, "fat-trait": {"Wide": 14}}
    for argv in modes:
        status, before, after, reads, measured, _ = invoke(argv, fallen)
        assert len(reads) == len(measured) == 1
        if argv in ([], ["--check"]):
            assert status == 1 and before == after, "D10d original shared fall requires recording"
        else:
            assert status == 0 and json.loads(after)["fat-trait"] == {"Wide": 13}, "D10d original fall records"
    for argv in (["--write-baseline"], ["--write-baseline", "--reason", "first record"]):
        for count, expected in ((0, 0), (13, 1)):
            status, before, after, reads, measured, _ = invoke(argv, None, held, count)
            assert status == expected and before is None and not reads and measured, "D10e absent record hard authority"
            assert (after is not None) == (expected == 0), "D10e only a hard-valid first record is written"
    for argv, previous in ((["--unknown"], held), ([], None), (["--tighten"], None)):
        status, before, after, reads, measured, _ = invoke(argv, previous)
        assert status in (1, 2) and before == after and not reads and not measured, "D10f admission before IO"


if __name__ == "__main__":
    raise SystemExit(_architecture_ratchet.main(
        sys.argv[1:], prog=Path(__file__).name, description=__doc__, rules=RULES, measure=_measure_workspace,
        baseline_path=BASELINE, self_test=selftest, hard_rules=_workspace_hard_errors))
