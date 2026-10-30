#!/usr/bin/env python3
"""Keep the Session acceptance ledger honest about what was proven.

The ledger in ``docs/design/session-execution-test-acceptance/`` binds each
scenario to tests and execution receipts. A green ledger must not be produced
by a name, a zero-assertion binding, a skipped or compile-only run, a receipt
whose source identity was rewritten, or a suite total. This checker decides
those questions from the ledger and the source it cites; it does not run any
test and it is not a second runner.

Decision table (each rule refuses with its code; ``--self-test`` executes every
refusal against a mutated copy of a minimal ledger):

| Code | Cause                                                      | Effect  |
| ---- | ---------------------------------------------------------- | ------- |
| L01  | counts disagree with the cases they summarize              | refuse  |
| L02  | a binding, group entrypoint or cited source path is absent | refuse  |
| L03  | a clause binding names a function its file does not define | refuse  |
| L04  | a clause binding cites no assertion, or a line without one | refuse  |
| L05  | a clause witness is skipped, compile-only or not passed    | refuse  |
| L06  | a witness disagrees with its receipt's source identity     | refuse  |
| L07  | a clause-complete case leaves an oracle or control unbound | refuse  |
| L08  | a clause-complete case does not list executed/open slices  | refuse  |
| L09  | acceptance claimed while a requirement or gate is open     | refuse  |
| L10  | a platform-pending case has no written reason              | refuse  |
| L11  | an evidence entry changed after it was registered          | refuse  |
| L12  | a product gap has no reason, follow-up or ignored reproducer | refuse |
| L13  | group, binding or evidence identities collide               | refuse |
| L14  | not-applicable without a reason or an executed refusal proof | refuse |
"""

from __future__ import annotations

import copy
import hashlib
import json
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LEDGER = Path("docs/design/session-execution-test-acceptance")

# Legacy states keep the meaning the calibration gave them. The two new states
# are the only ones a clause binding can produce.
LEGACY_STATES = frozenset({
    "design_only", "current_subset", "source_binding_only", "historical_subset",
    "aggregate_subset_only",
})
CLAUSE_COMPLETE = "current_clause_complete"
PLATFORM_PENDING = "platform_pending"
# The product does not yet satisfy an oracle: a reproducer exists, is ignored
# with its reason, and is never counted as execution evidence.
PRODUCT_GAP = "product_gap"
# The mode is not offered here; an executed test proves the capability is
# refused, never that the behaviour passed.
NOT_APPLICABLE = "not_applicable"
STATES = LEGACY_STATES | {CLAUSE_COMPLETE, PLATFORM_PENDING, PRODUCT_GAP, NOT_APPLICABLE}
CLAUSE_RELATION = "covers_case_clauses"
GAP_RELATION = "reproduces_product_gap"
# A Rust reproducer is `#[ignore = "product gap: ..."]`; a Python one is a
# `skip*` decorator whose reason starts the same way.
IGNORED = re.compile(r'#\[ignore\s*=\s*"product gap:|@unittest\.skip\w*\(.*"product gap:')
GATE_GROUPS = ("T00", "T34", "T35", "T36")
ACCEPTED = frozenset({"accepted_in_scope"})
# Text that proves something was checked. A helper whose name only says
# "check" does not count; the cited line has to be the assertion itself.
ASSERTION = re.compile(
    r"\b(?:assert(?:_eq|_ne|_matches)?!|prop_assert(?:_eq|_ne)?!|panic!|unreachable!|compile_fail|assert_\w+(?:::<[^>]*>)?\(|"
    r"self\.assert\w+\(|assert\s|assert\.\w+\(|expect\()"
)
COMPILE_ONLY = re.compile(r"(?:--no-run\b|\bcargo\s+(?:check|build)\b)")
# A Rust `fn` or Python `def`; a test named only by a string (TS/JS/shell)
# must still appear verbatim in its file.
DEFINITION = r"\b(?:fn|def)\s+{name}\s*[<(]"


def _load(root: Path) -> dict:
    ledger = root / LEDGER
    catalog = json.loads((ledger / "catalog.json").read_text(encoding="utf-8"))
    data = {"catalog": catalog, "groups": [], "bindings": [], "evidence_registry": []}
    for part in catalog["parts"]:
        value = json.loads((ledger / part["file"]).read_text(encoding="utf-8"))
        data[part["key"]] += value
    return data


def entry_digest(entry: dict) -> str:
    body = {key: value for key, value in entry.items() if key != "entry_sha256"}
    return hashlib.sha256(json.dumps(body, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def _cases(data: dict):
    for group in data["groups"]:
        for case in group["cases"]:
            yield group, case


def _source_lines(root: Path, path: str, cache: dict) -> list[str] | None:
    if path not in cache:
        file = root / path
        cache[path] = file.read_text(encoding="utf-8").splitlines() if file.is_file() else None
    return cache[path]


def _defines(lines: list[str], path: str, name: str) -> bool:
    regex = re.compile(DEFINITION.format(name=re.escape(name)))
    if any(regex.search(line) for line in lines):
        return True
    return not path.endswith((".rs", ".py")) and any(name in line for line in lines)


def _witness_errors(binding: dict, registry: dict, root: Path) -> list[str]:
    errors = []
    for witness in binding["execution_witnesses"]:
        evidence = registry.get(witness["evidence"])
        where = f"{binding['id']} witness {witness.get('case')}"
        if evidence is None:
            errors.append(f"L02 {where} cites unknown evidence {witness['evidence']}")
            continue
        if witness.get("source_head") and witness["source_head"] != evidence["source"]["head"]:
            errors.append(f"L06 {where} source {witness['source_head']} differs from "
                          f"{evidence['id']} {evidence['source']['head']}")
        if bool(witness.get("historical")) != bool(evidence.get("historical")):
            errors.append(f"L06 {where} historical flag differs from {evidence['id']}")
        if binding["relation"] != CLAUSE_RELATION:
            continue
        if witness.get("result") != "passed" or witness.get("skipped") or not evidence.get("passed"):
            errors.append(f"L05 {where} is not a passed, unskipped execution")
        if COMPILE_ONLY.search(" ".join(evidence.get("argv", []))):
            errors.append(f"L05 {where} rests on a compile-only command")
        if evidence.get("historical") and not witness.get("inheritance_review"):
            # A historical receipt is retained, never relabelled; inheriting it
            # needs a per-input review recorded beside the witness.
            errors.append(f"L06 {where} inherits historical {evidence['id']} without an input review")
        receipt = root / evidence.get("receipt", "")
        if evidence.get("receipt") and receipt.is_file():
            recorded = json.loads(receipt.read_text(encoding="utf-8"))
            if hashlib.sha256(receipt.read_bytes()).hexdigest() != evidence["receipt_sha256"]:
                errors.append(f"L06 {evidence['id']} receipt bytes differ from receipt_sha256")
            if recorded.get("source", {}).get("head") != evidence["source"]["head"]:
                errors.append(f"L06 {evidence['id']} receipt source differs from the registry")
            executed = set(recorded.get("executed", []))
            if witness["case"] not in executed:
                errors.append(f"L05 {where} is absent from the receipt's executed cases")
    return errors


def _binding_errors(binding: dict, registry: dict, root: Path, cache: dict) -> list[str]:
    errors = []
    for location in binding["source_locations"]:
        lines = _source_lines(root, location["path"], cache)
        if lines is None:
            errors.append(f"L02 {binding['id']} source {location['path']} is absent")
        elif not _defines(lines, location["path"], binding["name"]):
            errors.append(f"L03 {binding['id']} {location['path']} does not define {binding['name']}")
        elif binding["relation"] == CLAUSE_RELATION:
            line = location.get("line", 0)
            if not (0 < line <= len(lines)) or binding["name"] not in lines[line - 1]:
                errors.append(f"L03 {binding['id']} {location['path']}:{line} is not {binding['name']}")
    if binding["relation"] == GAP_RELATION:
        for location in binding["source_locations"]:
            lines = _source_lines(root, location["path"], cache) or []
            line = location.get("line", 0)
            window = lines[max(0, line - 4):line]
            if not any(IGNORED.search(text) for text in window):
                errors.append(f"L12 {binding['id']} reproducer is not ignored as a product gap")
        if binding["execution_witnesses"]:
            errors.append(f"L12 {binding['id']} counts a gap reproducer as execution evidence")
    if binding["relation"] == CLAUSE_RELATION:
        sites = binding.get("assertion_locations", [])
        if not sites:
            errors.append(f"L04 {binding['id']} cites no assertion")
        for site in sites:
            lines = _source_lines(root, site["path"], cache)
            line = site.get("line", 0)
            if lines is None or not (0 < line <= len(lines)) or not ASSERTION.search(lines[line - 1]):
                errors.append(f"L04 {binding['id']} {site['path']}:{line} is not an assertion")
        if not binding["execution_witnesses"]:
            errors.append(f"L05 {binding['id']} has no execution witness")
    return errors + _witness_errors(binding, registry, root)


def _case_errors(case: dict, bindings: dict, gates_closed: bool) -> list[str]:
    errors = []
    state = case["evidence_state"]
    if state not in STATES:
        errors.append(f"L01 {case['id']} has unknown evidence state {state}")
    for binding_id in case["binding_ids"]:
        if binding_id not in bindings:
            errors.append(f"L02 {case['id']} binds unknown {binding_id}")
    clause = [bindings[b] for b in case["binding_ids"]
              if b in bindings and bindings[b]["relation"] == CLAUSE_RELATION
              and case["id"] in bindings[b].get("covers", {})]
    if state == CLAUSE_COMPLETE:
        covered = {index for b in clause for index in b["covers"][case["id"]].get("oracles", [])}
        missing = sorted(set(range(len(case["oracles"]))) - covered)
        if missing:
            errors.append(f"L07 {case['id']} oracles {missing} have no clause binding")
        if not any(b["covers"][case["id"]].get("negative_control") for b in clause):
            errors.append(f"L07 {case['id']} negative control has no clause binding")
        slices = case.get("slices") or {}
        if not slices.get("executed") or "unaccepted" not in slices:
            errors.append(f"L08 {case['id']} does not list executed and unaccepted slices")
    elif state == PLATFORM_PENDING:
        if not case.get("platform_pending_reason"):
            errors.append(f"L10 {case['id']} is platform pending without a reason")
    elif state == NOT_APPLICABLE:
        executed_refusal = [b for b in clause if b["execution_witnesses"]]
        if not case.get("not_applicable_reason") or not executed_refusal:
            errors.append(f"L14 {case['id']} is not applicable without a reason or an executed refusal proof")
    elif state == PRODUCT_GAP:
        gap = case.get("product_gap") or {}
        reproducers = [bindings[b] for b in case["binding_ids"]
                       if b in bindings and bindings[b]["relation"] == GAP_RELATION]
        if not gap.get("reason") or not gap.get("follow_up") or not reproducers:
            errors.append(f"L12 {case['id']} product gap lacks a reason, follow-up or reproducer")
    elif clause:
        # A clause binding may support a subset; it never upgrades design_only.
        if state == "design_only":
            errors.append(f"L01 {case['id']} keeps design_only while bound to clause tests")
    if case["acceptance_state"] in ACCEPTED:
        open_slices = (case.get("slices") or {}).get("unaccepted", ["unknown"])
        if state != CLAUSE_COMPLETE or open_slices or not case.get("acceptance_scope") or not gates_closed:
            errors.append(f"L09 {case['id']} claims acceptance while a requirement is open")
    return errors


def ledger_errors(data: dict, root: Path = ROOT, frozen: dict | None = None) -> list[str]:
    # Refuse ambiguous identity before any dictionary can silently hide it.
    collisions = [f"L13 {kind} has duplicate identity {identity}"
                  for kind in ("groups", "bindings", "evidence_registry")
                  for identity, count in Counter(row["id"] for row in data[kind]).items()
                  if count > 1]
    if collisions:
        return sorted(collisions)
    bindings = {b["id"]: b for b in data["bindings"]}
    registry = {e["id"]: e for e in data["evidence_registry"]}
    cache: dict = {}
    errors = []
    tally = Counter(case["evidence_state"] for _group, case in _cases(data))
    counts = data["catalog"]["counts"]
    if dict(tally) != {k: v for k, v in counts["case_evidence_states"].items() if v}:
        errors.append(f"L01 case_evidence_states {counts['case_evidence_states']} != {dict(tally)}")
    if sum(tally.values()) != counts["scenarios"]:
        errors.append(f"L01 scenarios {counts['scenarios']} != {sum(tally.values())}")
    accepted = sum(case["acceptance_state"] in ACCEPTED for _g, case in _cases(data))
    if accepted != counts["fully_accepted_scenarios"]:
        errors.append(f"L01 fully_accepted_scenarios {counts['fully_accepted_scenarios']} != {accepted}")
    gates_closed = all(case["evidence_state"] == CLAUSE_COMPLETE
                       for group, case in _cases(data) if group["id"] in GATE_GROUPS)
    for group, case in _cases(data):
        if _source_lines(root, group["entrypoint"], cache) is None:
            errors.append(f"L02 {group['id']} entrypoint {group['entrypoint']} is absent")
        errors += _case_errors(case, bindings, gates_closed)
    for binding in data["bindings"]:
        errors += _binding_errors(binding, registry, root, cache)
    for entry in data["evidence_registry"]:
        recorded = entry.get("entry_sha256")
        if recorded is not None and recorded != entry_digest(entry):
            errors.append(f"L11 {entry['id']} changed after registration")
        if frozen and entry["id"] in frozen and frozen[entry["id"]] != entry_digest(entry):
            errors.append(f"L11 {entry['id']} differs from the committed registry")
    return sorted(dict.fromkeys(errors))


def committed_registry(root: Path, revision: str = "HEAD") -> dict:
    """Digest each evidence entry as the base revision recorded it."""
    path = (LEDGER / "evidence-registry.json").as_posix()
    shown = subprocess.run(["git", "-C", str(root), "show", f"{revision}:{path}"],
                           capture_output=True, check=False)
    if shown.returncode != 0:
        return {}
    return {e["id"]: entry_digest(e) for e in json.loads(shown.stdout.decode("utf-8"))}


def _fixture(root: Path) -> dict:
    (root / "src").mkdir(parents=True)
    (root / "src/lib.rs").write_text(
        "fn real_case() {\n    let x = 1;\n    assert_eq!(x, 1);\n}\n"
        "fn named_only() {\n    let _ = 1;\n}\n"
        "#[test]\n#[ignore = \"product gap: fixture\"]\nfn gap_case() {\n    assert!(false);\n}\n",
        encoding="utf-8")
    receipt = root / "target/receipt.json"
    receipt.parent.mkdir()
    receipt.write_text(json.dumps({"source": {"head": "a" * 40}, "executed": ["lib::real_case"]}),
                       encoding="utf-8")
    evidence = {"id": "E90", "receipt": "target/receipt.json",
                "receipt_sha256": hashlib.sha256(receipt.read_bytes()).hexdigest(),
                "source": {"head": "a" * 40}, "passed": True, "historical": False,
                "argv": ["cargo", "nextest", "run", "-E", "test(real_case)"]}
    evidence["entry_sha256"] = entry_digest(evidence)
    binding = {"id": "B900", "name": "real_case", "relation": CLAUSE_RELATION,
               "source_locations": [{"path": "src/lib.rs", "line": 1}],
               "assertion_locations": [{"path": "src/lib.rs", "line": 3}],
               "covers": {"T00-C01": {"oracles": [0, 1], "negative_control": True}},
               "execution_witnesses": [{"evidence": "E90", "case": "lib::real_case", "result": "passed",
                                        "skipped": False, "source_head": "a" * 40, "historical": False}]}
    case = {"id": "T00-C01", "oracles": ["o1", "o2"], "binding_ids": ["B900"],
            "evidence_state": CLAUSE_COMPLETE, "acceptance_state": "pending", "acceptance_scope": None,
            "slices": {"executed": ["windows-msvc"], "unaccepted": ["linux"]}}
    return {"catalog": {"counts": {"scenarios": 1, "fully_accepted_scenarios": 0,
                                   "case_evidence_states": {CLAUSE_COMPLETE: 1}}},
            "groups": [{"id": "T00", "entrypoint": "src/lib.rs", "cases": [case]}],
            "bindings": [binding], "evidence_registry": [evidence]}


def _mutations():
    """Each mutation breaks one invariant; the named code must refuse it."""
    def case(d): return d["groups"][0]["cases"][0]
    def bind(d): return d["bindings"][0]
    def wit(d): return bind(d)["execution_witnesses"][0]

    def rename(d): bind(d)["name"] = "renamed_case"
    def name_only(d):
        bind(d)["name"] = "named_only"
        bind(d)["source_locations"][0]["line"] = 5
        bind(d)["assertion_locations"] = []
    def fake_site(d):
        bind(d)["name"] = "named_only"
        bind(d)["source_locations"][0]["line"] = 5
        bind(d)["assertion_locations"] = [{"path": "src/lib.rs", "line": 6}]
    def skipped(d): wit(d)["skipped"] = True
    def no_run(d):
        e = d["evidence_registry"][0]
        e["argv"].append("--no-run")
        e["entry_sha256"] = entry_digest(e)
    def rewritten(d):
        e = d["evidence_registry"][0]
        e["source"]["head"] = "b" * 40
        wit(d)["source_head"] = "b" * 40
        e["entry_sha256"] = entry_digest(e)
    def edited(d): d["evidence_registry"][0]["source"]["head"] = "c" * 40
    def inherited(d):
        e = d["evidence_registry"][0]
        e["historical"] = True
        wit(d)["historical"] = True
        e["entry_sha256"] = entry_digest(e)
    def unexecuted(d): wit(d)["case"] = "lib::other_case"
    def oracle_gap(d): bind(d)["covers"]["T00-C01"]["oracles"] = [0]
    def control_gap(d): bind(d)["covers"]["T00-C01"]["negative_control"] = False
    def slices_gap(d): case(d)["slices"] = {"executed": []}
    def accepted(d):
        case(d)["acceptance_state"] = "accepted_in_scope"
        d["catalog"]["counts"]["fully_accepted_scenarios"] = 1
    def count_total(d): d["catalog"]["counts"]["case_evidence_states"] = {CLAUSE_COMPLETE: 2}
    def pending_reason(d): case(d)["evidence_state"] = PLATFORM_PENDING; \
        d["catalog"]["counts"]["case_evidence_states"] = {PLATFORM_PENDING: 1}
    def upgraded(d): case(d)["evidence_state"] = "design_only"; \
        d["catalog"]["counts"]["case_evidence_states"] = {"design_only": 1}
    def missing_path(d): bind(d)["source_locations"][0]["path"] = "src/moved.rs"
    def gap_unignored(d):
        case(d)["evidence_state"] = PRODUCT_GAP
        case(d)["product_gap"] = {"reason": "r", "follow_up": "f"}
        d["catalog"]["counts"]["case_evidence_states"] = {PRODUCT_GAP: 1}
        bind(d)["relation"] = GAP_RELATION
    def na_unproven(d):
        case(d)["evidence_state"] = NOT_APPLICABLE
        case(d)["not_applicable_reason"] = "mode not offered"
        d["catalog"]["counts"]["case_evidence_states"] = {NOT_APPLICABLE: 1}
        bind(d)["execution_witnesses"] = []

    def gap_unreasoned(d):
        gap_unignored(d)
        case(d)["product_gap"] = {"reason": "r"}
    return (("L03", rename), ("L04", name_only), ("L04", fake_site), ("L05", skipped),
            ("L05", no_run), ("L06", rewritten), ("L11", edited), ("L06", inherited), ("L05", unexecuted),
            ("L07", oracle_gap), ("L07", control_gap), ("L08", slices_gap), ("L09", accepted),
            ("L01", count_total), ("L10", pending_reason), ("L01", upgraded), ("L02", missing_path),
            ("L12", gap_unignored), ("L12", gap_unreasoned), ("L14", na_unproven))


def self_test() -> int:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        base = _fixture(root)
        frozen = {e["id"]: entry_digest(e) for e in base["evidence_registry"]}
        positive = ledger_errors(base, root, frozen)
        if positive:
            print(f"self-test: the valid ledger was refused: {positive}", file=sys.stderr)
            return 1
        failures = 0
        for code, mutate in _mutations():
            data = copy.deepcopy(base)
            mutate(data)
            found = ledger_errors(data, root, frozen)
            if not any(error.startswith(code) for error in found):
                print(f"self-test: {mutate.__name__} was not refused with {code}: {found}", file=sys.stderr)
                failures += 1
        # Acceptance while every requirement is met and the gates are closed is
        # the one admitted claim; an open slice keeps it refused (above).
        data = copy.deepcopy(base)
        data["groups"][0]["cases"][0].update(acceptance_state="accepted_in_scope",
                                             acceptance_scope="windows-msvc", slices={"executed": ["w"], "unaccepted": []})
        data["catalog"]["counts"]["fully_accepted_scenarios"] = 1
        if ledger_errors(data, root, frozen):
            print("self-test: a fully closed case was refused", file=sys.stderr)
            failures += 1
        # A recorded gap with an ignored reproducer and no evidence is admitted.
        data = copy.deepcopy(base)
        case = data["groups"][0]["cases"][0]
        case.update(evidence_state=PRODUCT_GAP, product_gap={"reason": "r", "follow_up": "f"})
        data["catalog"]["counts"]["case_evidence_states"] = {PRODUCT_GAP: 1}
        data["bindings"][0].update(name="gap_case", relation=GAP_RELATION, execution_witnesses=[],
                                   source_locations=[{"path": "src/lib.rs", "line": 10}])
        if ledger_errors(data, root, frozen):
            print(f"self-test: a recorded product gap was refused: {ledger_errors(data, root, frozen)}", file=sys.stderr)
            failures += 1
    return 1 if failures else 0


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return self_test()
    if argv:
        print("usage: check_session_acceptance.py [--self-test]", file=sys.stderr)
        return 2
    errors = ledger_errors(_load(ROOT), ROOT, committed_registry(ROOT))
    for error in errors:
        print(f"session-acceptance: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
