"""Code identifiers name responsibility, not mechanism; four ratchets that only shrink.

ADR-0091 D7 (plan 9.15, W30). The documentation vocabulary is checked by
`check_architecture_vocabulary.py`; this module checks code identifiers. Counted
from production code (comments, string literals and `cfg(test)` code are not
scanned); a type is any `struct`, `enum`, `trait`, `type` or `union` declaration:

* `mechanism-name`: a type whose name ends in a mechanism word
  (`MECHANISM_WORDS`, compared with the last CamelCase word of the name) outside
  a composition root -- a crate under `crates/bin/` or of layer `bootstrap`,
  where wiring is the responsibility (D7.3). Test doubles (`TEST_DOUBLE_PREFIXES`)
  are not counted. Keyed by type name.
* `bare-runtime`: a type named exactly `Runtime` outside the SDK (crates of
  product `sdk`): "Runtime" means the SDK engine (D7.2). Keyed `crate::Runtime`.
* `bare-name`: a type named exactly `Provider`, `Coordinator` or `Host`,
  anywhere: each word has one qualified meaning (D7.4). Keyed `crate::Name`.
* `turn-noun`: an identifier with the word `Turn`/`turn` (or its plural) as one
  of its CamelCase or snake_case words outside the protocol adapters
  (`awaken-protocol-*`, and the crates that speak an external protocol's own
  vocabulary: the ACP executor and the Managed session-event projection): the
  run loop speaks Thread, Run and Step (ADR-0064 D1).
  The letters inside another word (`return`, `Return`) are not the word. Keyed
  `crate::identifier`.

Every ratchet is held at or below `naming_baseline.json`; see
`_architecture_ratchet.py` for how a fall is recorded
(`python3 scripts/ci/_naming_fitness.py --write-baseline`).
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

BASELINE = Path(__file__).with_name("naming_baseline.json")

MECHANISM_WORDS = frozenset({
    "Ports", "Port", "Adapters", "Wiring", "Dependencies", "Deps", "Bindings", "Handles", "Services", "Support",
    "Parts", "Host", "Manager", "Helper", "Helpers", "Core", "Base", "Common", "Shared", "Util", "Utils",
})
TEST_DOUBLE_PREFIXES = ("Fake", "Mock", "Stub", "Dummy", "Spy", "Test")
BARE_NAMES = frozenset({"Provider", "Coordinator", "Host"})
TURN_WORDS = frozenset({"turn", "turns"})
PROTOCOL_ADAPTER_PREFIX = "awaken-protocol-"
# Crates outside the prefix whose `turn` words are an external protocol's own
# vocabulary (ACP `max_turn_requests`, Managed `end_turn`), which ADR-0064 D1
# allows in protocol DTOs and adapters.
PROTOCOL_ADAPTER_CRATES = frozenset({"awaken-run-executor-acp", "awaken-managed-session-events"})
SDK_PRODUCT = "sdk"
COMPOSITION_LAYER = "bootstrap"
BINARY_DIRECTORY = "bin"

RULES = (
    Rule("mechanism-name", "name the type for what its consumer does with it, or split it; mechanism words belong "
         "in composition roots (ADR-0091 D7.3, D7.5)"),
    Rule("bare-runtime", "\"Runtime\" means the SDK engine; name the type for its responsibility (ADR-0091 D7.2)"),
    Rule("bare-name", "qualify the name (`SandboxProvider`, `ModelProvider`, ...); `Coordinator` is the "
         "Coordinator process (ADR-0091 D7.4)"),
    Rule("turn-noun", "use Thread, Run or Step (ADR-0064 D1); `turn` belongs to protocol DTOs and adapters "
         "(ADR-0091 D7.0)"),
)

_DECLARATION = re.compile(r"\b(?:struct|enum|trait|type|union)\s+([A-Z]\w*)")
_IDENTIFIER = re.compile(r"\b[A-Za-z_]\w*\b")
_WORD = re.compile(r"[A-Z]+(?![a-z])|[A-Z][a-z0-9]*|[a-z0-9]+")


def words(identifier: str) -> list[str]:
    """CamelCase and snake_case words of an identifier, in order."""
    return [word for chunk in identifier.split("_") for word in _WORD.findall(chunk)]


def is_mechanism_name(name: str) -> bool:
    parts = words(name)
    return bool(parts) and parts[-1] in MECHANISM_WORDS and not name.startswith(TEST_DOUBLE_PREFIXES)


def has_turn_noun(identifier: str) -> bool:
    return any(word.lower() in TURN_WORDS for word in words(identifier))


def composition_root(crate: _architecture_ratchet.CrateSource) -> bool:
    return crate.metadata.get("layer") == COMPOSITION_LAYER or crate.directory.parent.name == BINARY_DIRECTORY


def measure(sources: tuple[_architecture_ratchet.CrateSource, ...], root: Path) -> tuple[Counts, Places]:
    counts: Counts = {rule.name: Counter() for rule in RULES}
    places: Places = {rule.name: defaultdict(list) for rule in RULES}

    def record(rule: str, key: str, where: str) -> None:
        counts[rule][key] += 1
        places[rule][key].append(where)

    for crate in sources:
        mechanism_allowed = composition_root(crate)
        sdk = crate.metadata.get("product") == SDK_PRODUCT
        adapter = (
            crate.name.startswith(PROTOCOL_ADAPTER_PREFIX)
            or crate.name in PROTOCOL_ADAPTER_CRATES
        )
        for path, syntax in crate.syntax.items():
            relative = path.relative_to(root) if root in path.parents else path
            for match in _DECLARATION.finditer(syntax):
                name, where = match.group(1), f"{relative}:{_architecture_ratchet.line_of(syntax, match.start())}"
                if not mechanism_allowed and is_mechanism_name(name):
                    record("mechanism-name", name, where)
                if name == "Runtime" and not sdk:
                    record("bare-runtime", f"{crate.name}::{name}", where)
                if name in BARE_NAMES:
                    record("bare-name", f"{crate.name}::{name}", where)
            if adapter:
                continue
            seen: set[str] = set()
            for match in _IDENTIFIER.finditer(syntax):
                identifier = match.group(0)
                if identifier not in seen and has_turn_noun(identifier):
                    seen.add(identifier)
                    record("turn-noun", f"{crate.name}::{identifier}",
                           f"{relative}:{_architecture_ratchet.line_of(syntax, match.start())}")
    return ({rule: dict(found) for rule, found in counts.items()},
            {rule: dict(found) for rule, found in places.items()})


def _measure_workspace(crates: Path = _crate_boundary_workspace.CRATES) -> tuple[Counts, Places]:
    return measure(_architecture_ratchet.crate_sources(crates), _crate_boundary_workspace.REPO_ROOT)


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    counts, places = _measure_workspace(crates)
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    return [f"naming: {error}"
            for error in _architecture_ratchet.compare(RULES, counts, places, baseline, baseline_path.name)]


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                  | effect |
    | N1   | type ending in a mechanism word outside a composition root             | counted by name |
    | N2   | the same in a `crates/bin/*` or `bootstrap` crate; a test double; the word mid-name | not counted |
    | N3   | type named `Runtime` outside / inside the SDK                          | counted / not counted |
    | N4   | type named `Provider`, `Coordinator`, `Host` anywhere; `ModelProvider` | counted; not counted |
    | N5   | identifier with the word turn: `TurnId`, `max_turns`, `END_TURN`       | counted once per file |
    | N6   | `return`, `Return`, `turnover`, a string or comment, a protocol adapter | not counted |
    """
    assert words("HTTPSharedHost_turn_id") == ["HTTP", "Shared", "Host", "turn", "id"], "words"
    assert is_mechanism_name("SharedHost") and is_mechanism_name("ResourcePorts"), "N1"
    assert not is_mechanism_name("HostAddress") and not is_mechanism_name("FakeHost"), "N2"
    assert has_turn_noun("TurnId") and has_turn_noun("max_turns") and has_turn_noun("END_TURN"), "N5"
    assert not has_turn_noun("return_value") and not has_turn_noun("ReturnCode") and not has_turn_noun("turnover"), \
        "N6"

    code = (
        "pub struct SharedHost;\npub trait McpManager {}\nstruct HostAddress;\nstruct FakeHost;\n"
        "type Runtime = u8;\nstruct Provider;\nenum ModelProvider {}\n"
        "fn f(turn_id: TurnId) -> Option<u8> { let x = \"max_turns\"; return None; }\n// TurnComment\n"
        "fn g(turn_id: u8) {}\n"
    )

    def crate(name, directory, **metadata):
        return _architecture_ratchet.CrateSource(name, metadata, Path(directory), {Path(directory) / "src/lib.rs": code})

    counts, places = measure((
        crate("awaken-x", "/w/crates/server/awaken-x", product="platform"),
        crate("awaken-cli", "/w/crates/bin/awaken-cli", product="apps", layer="bootstrap"),
        crate("awaken-sdk", "/w/crates/runtime/awaken-sdk", product="sdk"),
        crate("awaken-protocol-y", "/w/crates/server/awaken-protocol-y", product="platform"),
    ), Path("/w"))
    assert counts["mechanism-name"] == {"SharedHost": 3, "McpManager": 3}, f"N1/N2 {counts['mechanism-name']}"
    assert counts["bare-runtime"] == {"awaken-x::Runtime": 1, "awaken-cli::Runtime": 1,
                                      "awaken-protocol-y::Runtime": 1}, "N3"
    assert counts["bare-name"]["awaken-sdk::Provider"] == 1 and len(counts["bare-name"]) == 4, "N4"
    assert set(counts["turn-noun"]) == {f"{name}::{identifier}" for name in ("awaken-x", "awaken-cli", "awaken-sdk")
                                        for identifier in ("turn_id", "TurnId")}, f"N5/N6 {counts['turn-noun']}"
    assert places["turn-noun"]["awaken-x::turn_id"] == ["crates/server/awaken-x/src/lib.rs:8"], "N5 once per file"
    print("_naming_fitness selftest: the decision tables hold")


if __name__ == "__main__":
    raise SystemExit(_architecture_ratchet.main(
        sys.argv[1:], prog=Path(__file__).name, description=__doc__, rules=RULES, measure=_measure_workspace,
        baseline_path=BASELINE, self_test=selftest))
