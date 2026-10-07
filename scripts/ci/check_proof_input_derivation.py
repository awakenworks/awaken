#!/usr/bin/env python3
"""Refuse a Kani proof whose inputs are not the ones production derives.

A harness can be green and constrain nothing.  Three shapes do that, and all
three have reached `main`:

* **A `#[cfg(kani)]` fork inside a production function.**  `ModelBinding::new`
  built its struct literal directly under Kani because the real admission was
  proved separately, so *every* proof whose property depended on a binding
  having been admitted was vacuous.  A proof-only altitude belongs in a
  dedicated bounded representative (`McpAttachmentContentDigest::
  proof_representative`), never inside the door production calls.
* **A tautology.**  `exact_session_work_receipt_rejects_every_changed_
  coordinate` asserted `exact_session_work_coordinate(a..f) == a && .. && f`
  -- the callee's own body, restated.  Such an assertion cannot fail for any
  edit to that body, so it is a harness with no theorem in it.
* **A free symbol standing in for a derived fact.**  The settlement harness
  took `inputs_allow_idle` as a free `kani::any()` bool while production
  derives it as `all_events_processed || only_blocked_users`.  The predicate a
  real bug fix changed therefore sat outside the proof, and the harness held
  identically before and after the fix.

The third shape has legitimate instances: a genuinely environmental input (a
clock, a wire byte, an operator choice) is free by nature.  Those live in
`formal/proof-input-exceptions.json` under ``justified`` with a written reason
and the obligation the reason belongs to.  Everything this sweep found and
nobody has reviewed is a debt, written on the harness itself as
``/// @proof-input-debt <rule> <kernel> [<parameter>]``: not a licence, a
ceiling.  A finding that is neither justified nor tagged fails the gate, and a
tag or entry that no longer matches a finding fails too.  The debt can only
shrink, with no stored ceiling: the pre-commit hook refuses a staged debt HEAD
does not already carry, and ``--debt-ratchet`` refuses more debts at HEAD than
at its merge-base (failing closed when that history is missing).

Modes: ``--require-complete`` walks the tree, ``--staged <paths>`` asks the
same question about the given files, ``--survey`` prints the classification.
"""

from __future__ import annotations

import argparse
import collections
import contextlib
import io
import json
import pathlib
import re
import subprocess
import sys
from unittest import mock as unittest_mock

import _crate_boundary_workspace
import formal_anchors
import formal_ledger


# Input set: every Rust source it parses (harnesses and the production kernels
# they call live in the same files), the obligation ledger that names which
# files are production entrypoints, and the exception registry itself.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "formal/coverage.toml",
    "formal/obligations/**/*",
    "web/src/**/*",
    "formal/proof-input-exceptions.json",
    "scripts/ci/formal_ledger.py",
)

ROOT = pathlib.Path(__file__).resolve().parents[2]
REGISTRY = ROOT / "formal" / "proof-input-exceptions.json"
RULES = ("kani_fork", "tautology", "free_symbol")
# A debt: a finding nobody has reviewed yet, recorded on the harness itself.
DEBT_TAG = "proof-input-debt"
DEBT_LINE = re.compile(r"^\s*///\s*@proof-input-debt\s+(?P<debt>\S.*?)\s*$")
FIELDS = ("rule", "source", "harness", "kernel", "parameter", "derived_in")


def fail(message: str) -> None:
    print(f"proof input derivation: {message}", file=sys.stderr)
    raise SystemExit(1)


# ---------------------------------------------------------------------------
# A small Rust reader.  Only three shapes are needed -- function items, the
# bodies of `#[kani::proof]` items, and call argument lists -- so this stays a
# brace/paren matcher over the comment-blanked source rather than a parser.
# ---------------------------------------------------------------------------

FUNCTION = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:<[^>{(]*>)?\s*\(")
PROOF_ATTRIBUTE = re.compile(r"#\[kani::proof[^\]]*\]")
ASSERTION = re.compile(r"\bassert(?:_eq|_ne)?!\s*\(")
ASSERT_EQ = re.compile(r"\bassert_eq!\s*\(")
FREE_SYMBOL = re.compile(
    r"\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(?::[^=;]+)?"
    r"=\s*kani::any(?:::<[^>]*>)?\s*\(\s*\)\s*;"
)
# What makes a production argument a *derived fact* rather than forwarding.
# `lease.epoch` or `now_ms` hands the kernel a value it stores; a free symbol
# of the same type is then a faithful over-approximation of that value.  A
# comparison, a Boolean combination, a `matches!` or an `is_*`/`contains`
# predicate is a decision the production code computes, and a free bool in its
# place deletes that decision from the theorem -- which is exactly how the
# settlement harness survived the `all_events_processed || only_blocked_users`
# fix unchanged.
DERIVED = re.compile(
    r"==|!=|&&|\|\||[<>]=?|\bmatches!|\.is_[a-z_]+\s*\(|\.contains\w*\s*\(|\.any\s*\(|\.all\s*\("
)
KANI_FORK = re.compile(r"#\[cfg\(kani\)\]")
# A turbofish carries `<` and `>` without being a comparison: `None::<u64>` is
# a literal, not a derived fact.
TURBOFISH = re.compile(r"::<[^<>]*>")


def is_derived(expression: str) -> bool:
    """Whether a production argument computes a fact rather than forwarding one."""
    return DERIVED.search(TURBOFISH.sub("", expression)) is not None


def balanced(source: str, start: int, opening: str, closing: str) -> int:
    """Index just past the delimiter matching the one at ``start``."""
    depth = 0
    index = start
    while index < len(source):
        character = source[index]
        if character == opening:
            depth += 1
        elif character == closing:
            depth -= 1
            if depth == 0:
                return index + 1
        index += 1
    return len(source)


def arguments(source: str) -> list[str]:
    """Split one comma-separated argument or parameter list."""
    collected: list[str] = []
    depth = 0
    current: list[str] = []
    for character in source:
        if character in "([{":
            depth += 1
        elif character in ")]}":
            depth -= 1
        elif character == "," and depth == 0:
            if "".join(current).strip():
                collected.append("".join(current).strip())
            current = []
            continue
        current.append(character)
    if "".join(current).strip():
        collected.append("".join(current).strip())
    return collected


def functions(source: str):
    """Yield ``(name, parameters, body, item_start)`` for every ``fn`` item."""
    for match in FUNCTION.finditer(source):
        open_paren = match.end() - 1
        close_paren = balanced(source, open_paren, "(", ")")
        parameters = source[open_paren + 1 : close_paren - 1]
        brace = source.find("{", close_paren)
        semicolon = source.find(";", close_paren)
        if brace < 0 or 0 <= semicolon < brace:
            continue  # a trait method declaration has no body
        end = balanced(source, brace, "{", "}")
        yield match.group(1), parameters, source[brace + 1 : end - 1], match.start()


def harnesses(source: str):
    """Yield ``(name, body)`` for every ``#[kani::proof]`` function."""
    for match in PROOF_ATTRIBUTE.finditer(source):
        rest = source[match.end() :]
        signature = FUNCTION.search(rest)
        if signature is None:
            continue
        if re.sub(r"#\[[^\]]*\]|\s", "", rest[: signature.start()]):
            continue  # something other than attributes sits between
        brace = rest.find("{", signature.end())
        if brace < 0:
            continue
        end = balanced(rest, brace, "{", "}")
        yield signature.group(1), rest[brace + 1 : end - 1]


def parameter_names(parameters: str) -> list[str]:
    names = []
    for parameter in arguments(parameters):
        stripped = parameter.strip()
        if stripped in {"self", "&self", "&mut self", "mut self"}:
            names.append("self")
            continue
        names.append(stripped.split(":")[0].strip().removeprefix("mut ").strip())
    return names


def calls(source: str, name: str):
    """Yield the argument list of every plain call of ``name`` in ``source``."""
    pattern = re.compile(r"(?<![A-Za-z0-9_.])" + re.escape(name) + r"\s*\(")
    for match in pattern.finditer(source):
        if source[: match.start()].rstrip().endswith("fn"):
            continue
        open_paren = match.end() - 1
        close_paren = balanced(source, open_paren, "(", ")")
        yield arguments(source[open_paren + 1 : close_paren - 1])


def single_expression_body(body: str) -> str | None:
    """The body of a function that is one expression over its parameters."""
    stripped = body.strip()
    if not stripped or stripped.endswith(";"):
        return None
    if "let " in stripped or "return " in stripped:
        return None
    return stripped


def collapse(expression: str) -> str:
    return re.sub(r"\s+", "", expression).rstrip(",")


def substitute(expression: str, binding: dict[str, str]) -> str:
    return re.sub(
        r"[A-Za-z_][A-Za-z0-9_]*",
        lambda word: binding.get(word.group(0), word.group(0)),
        expression,
    )


def assert_eq_pairs(body: str):
    for match in ASSERT_EQ.finditer(body):
        open_paren = match.end() - 1
        close_paren = balanced(body, open_paren, "(", ")")
        parts = arguments(body[open_paren + 1 : close_paren - 1])
        if len(parts) >= 2:
            yield parts[0], parts[1]


# ---------------------------------------------------------------------------
# The three rules.
# ---------------------------------------------------------------------------


def rust_sources() -> list[pathlib.Path]:
    return sorted(
        path
        for path in (ROOT / "crates").rglob("*.rs")
        if "target" not in path.parts and "node_modules" not in path.parts
    )


def load_ledger() -> dict:
    try:
        return formal_ledger.load(ROOT)
    except formal_ledger.LedgerError as error:
        fail(str(error))


def production_entrypoints() -> set[str]:
    ledger = load_ledger()
    entrypoints: set[str] = set()
    for obligation in ledger["obligations"]:
        entrypoints.update(obligation.get("production_entrypoints") or ())
    return entrypoints


def crate_of(path: pathlib.Path) -> str:
    """The crate directory a source belongs to, for same-crate reachability."""
    parts = path.relative_to(ROOT).parts
    return "/".join(parts[:3]) if len(parts) > 3 else "/".join(parts)


def call_site_index(
    sources: dict[pathlib.Path, str],
) -> dict[str, list[tuple[pathlib.Path, list[str]]]]:
    """Every plain call in the tree, by callee name, with its file."""
    index: dict[str, list[tuple[pathlib.Path, list[str]]]] = {}
    pattern = re.compile(r"(?<![A-Za-z0-9_.:])([A-Za-z_][A-Za-z0-9_]*)\s*\(")
    keywords = {"if", "match", "while", "for", "fn", "return", "let", "assert"}
    for source_path, text in sources.items():
        for match in pattern.finditer(text):
            name = match.group(1)
            if name in keywords:
                continue
            # `fn name(` is a declaration; its parameter list is types, not a
            # derived argument. Without this, `current: Option<i64>` read as a
            # production derivation of a parameter called `current`.
            if text[: match.start()].rstrip().endswith("fn"):
                continue
            open_paren = match.end() - 1
            close_paren = balanced(text, open_paren, "(", ")")
            index.setdefault(name, []).append(
                (source_path, arguments(text[open_paren + 1 : close_paren - 1]))
            )
    return index


def kani_fork_findings(
    path: pathlib.Path, text: str, entrypoints: set[str]
) -> list[dict[str, str]]:
    """Rule 1: a `#[cfg(kani)]` conditional inside a production function body."""
    relative = path.relative_to(ROOT).as_posix()
    if relative not in entrypoints:
        return []
    found = []
    for name, _parameters, body, start in functions(text):
        if "kani::proof" in text[max(0, start - 200) : start]:
            continue
        if KANI_FORK.search(body) and "kani::proof" not in body:
            found.append(
                {
                    "rule": "kani_fork",
                    "source": relative,
                    "harness": name,
                    "kernel": name,
                    "parameter": "",
                    "derived_in": "same_crate",
                }
            )
    return found


def harness_findings(
    path: pathlib.Path,
    text: str,
    index: dict[str, list[tuple[pathlib.Path, list[str]]]],
    owners: dict[str, set[str]] | None = None,
) -> list[dict[str, str]]:
    """Rules 2 and 3, per `#[kani::proof]` harness in one file."""
    owners = owners or {}
    relative = path.relative_to(ROOT).as_posix()
    local: dict[str, tuple[str, str]] = {}
    for name, parameters, body, _start in functions(text):
        local.setdefault(name, (parameters, body))

    found: list[dict[str, str]] = []
    for harness, body in harnesses(text):
        restated = set()
        for actual, expected in assert_eq_pairs(body):
            head = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)\s*\(", actual.strip())
            if head is None:
                continue
            callee = head.group(1)
            if callee == harness or callee not in local:
                continue
            parameters, callee_body = local[callee]
            expression = single_expression_body(callee_body)
            if expression is None:
                continue
            names = parameter_names(parameters)
            supplied = next(iter(calls(actual, callee)), None)
            if supplied is None or len(supplied) != len(names):
                continue
            if collapse(substitute(expression, dict(zip(names, supplied)))) == collapse(
                expected
            ):
                restated.add(callee)
        # A harness whose every assertion restates a callee body proves nothing.
        # One restated assertion beside real ones is dead weight, not vacuity.
        if restated and len(restated) >= len(ASSERTION.findall(body)):
            found.append(
                {
                    "rule": "tautology",
                    "source": relative,
                    "harness": harness,
                    "kernel": sorted(restated)[0],
                    "parameter": "",
                    "derived_in": "same_crate",
                }
            )
            continue

        free = set(FREE_SYMBOL.findall(body))
        if not free:
            continue
        offenders = set()
        for callee, (parameters, _callee_body) in local.items():
            if callee == harness:
                continue
            names = parameter_names(parameters)
            for supplied in calls(body, callee):
                if len(supplied) != len(names):
                    continue
                for position, argument in enumerate(supplied):
                    if argument.strip() not in free:
                        continue
                    unique = len(owners.get(callee, {""})) == 1
                    for where, elsewhere in index.get(callee, ()):
                        if len(elsewhere) != len(names):
                            continue
                        if crate_of(where) != crate_of(path) and not unique:
                            continue  # the name is not a sound resolution
                        if collapse(",".join(elsewhere)) == collapse(",".join(supplied)):
                            continue  # the harness's own call site
                        if is_derived(elsewhere[position].strip()):
                            # Where the derivation lives decides who can fix it:
                            # a harness can only reach a same-crate derivation,
                            # so a cross-crate one needs a proof in that crate.
                            offenders.add(
                                (
                                    callee,
                                    names[position],
                                    "same_crate"
                                    if crate_of(where) == crate_of(path)
                                    else "other_crate",
                                )
                            )
                            break
        for callee, parameter, reach in sorted(offenders):
            found.append(
                {
                    "rule": "free_symbol",
                    "source": relative,
                    "harness": harness,
                    "kernel": callee,
                    "parameter": parameter,
                    "derived_in": reach,
                }
            )
    return found


def definition_crates(sources: dict[pathlib.Path, str]) -> dict[str, set[str]]:
    """Which crates define a function of each name.

    The call index keys on the bare name, which is only a sound resolution when
    one crate owns that name. `new`, `from` and `select` are defined in dozens
    of crates, and matching them across crate lines reported a *type* in an
    unrelated signature as a production derivation.
    """
    owners: dict[str, set[str]] = {}
    for path, text in sources.items():
        for name, _parameters, _body, _start in functions(text):
            owners.setdefault(name, set()).add(crate_of(path))
    return owners


def findings(paths: list[pathlib.Path] | None = None) -> list[dict[str, str]]:
    everything = rust_sources()
    sources = {
        path: _crate_boundary_workspace.rust_without_comments(
            path.read_text(errors="replace")
        )
        for path in everything
    }
    index = call_site_index(sources)
    owners = definition_crates(sources)
    entrypoints = production_entrypoints()
    selected = everything if paths is None else [p for p in paths if p in sources]
    collected: list[dict[str, str]] = []
    for path in selected:
        text = sources[path]
        collected.extend(kani_fork_findings(path, text, entrypoints))
        if "kani::proof" in text:
            collected.extend(harness_findings(path, text, index, owners))
    return collected


def key(finding: dict[str, str]) -> tuple[str, ...]:
    return (
        finding["rule"],
        finding["source"],
        finding["harness"],
        finding["kernel"],
        finding["parameter"],
    )


def validate_registry(
    registry: dict, obligations: set[str]
) -> tuple[dict[tuple[str, ...], dict], dict[tuple[str, ...], dict]]:
    """Every shape rule the registry itself must satisfy."""
    justified: dict[tuple[str, ...], dict] = {}
    backlog: dict[tuple[str, ...], dict] = {}
    if "backlog" in registry:
        fail(f"the registry holds justified entries only; a debt is a `/// @{DEBT_TAG}` tag on its harness")
    for section, target in (("justified", justified), ("backlog", backlog)):
        for entry in registry.get(section, ()):
            missing = {"rule", "source", "harness", "kernel", "parameter"} - entry.keys()
            if missing:
                fail(f"{section} entry missing {sorted(missing)}: {entry}")
            if entry["rule"] not in RULES:
                fail(f"{section} entry has unknown rule {entry['rule']!r}")
            if section == "justified":
                # A legitimately free symbol is a claim about the world, so it
                # carries its reason and the obligation that reason belongs to.
                if not entry.get("reason", "").strip():
                    fail(f"justified entry {entry['harness']} has no written reason")
                if entry.get("obligation") not in obligations:
                    fail(
                        f"justified entry {entry['harness']} names obligation "
                        f"{entry.get('obligation')!r}, which the ledger does not carry"
                    )
            if key(entry) in target:
                fail(f"duplicate {section} entry for {entry['harness']}")
            target[key(entry)] = entry
    overlap = justified.keys() & backlog.keys()
    if overlap:
        fail(f"{len(overlap)} entries are both justified and backlogged")
    return justified, backlog


def load_registry() -> tuple[dict[tuple[str, ...], dict], dict[tuple[str, ...], dict]]:
    """The justified entries (registry file) and the debts (harness tags)."""

    if not REGISTRY.exists():
        fail(f"{REGISTRY.relative_to(ROOT)} is missing")
    obligations = {
        obligation["id"] for obligation in load_ledger()["obligations"]
    }
    justified, _ = validate_registry(json.loads(REGISTRY.read_text()), obligations)
    debts, errors = debt_tags(formal_anchors.scan(ROOT).harnesses)
    if errors:
        fail("\n".join(errors))
    overlap = justified.keys() & debts.keys()
    if overlap:
        fail(f"{len(overlap)} findings are both justified and tagged @{DEBT_TAG}")
    return justified, debts


def debt_tags(harnesses: list) -> tuple[dict[tuple[str, ...], dict], list[str]]:
    """Each harness's own debts: `/// @proof-input-debt <rule> <kernel> [<parameter>]`.

    The tag sits on the harness the finding is about, so it moves with the
    harness; the kernel is named because one harness may leave the same
    parameter free for two kernels.
    """

    debts: dict[tuple[str, ...], dict] = {}
    errors: list[str] = []
    for harness in harnesses:
        for anchor in harness.values(DEBT_TAG):
            parts = anchor.value.split()
            if not (
                (len(parts) == 3 and parts[0] == "free_symbol")
                or (len(parts) == 2 and parts[0] == "tautology")
            ):
                errors.append(
                    f"{anchor.where}: expected `/// @{DEBT_TAG} free_symbol <kernel> <parameter>` "
                    f"or `/// @{DEBT_TAG} tautology <kernel>`, got {anchor.value!r}"
                )
                continue
            entry = {
                "rule": parts[0], "source": harness.path, "harness": harness.name,
                "kernel": parts[1], "parameter": parts[2] if len(parts) == 3 else "",
                "where": anchor.where,
            }
            if key(entry) in debts:
                errors.append(f"{anchor.where}: repeats the debt at {debts[key(entry)]['where']}")
            debts[key(entry)] = entry
    return debts, errors


def debt_text(line: str) -> str | None:
    """The location-free identity of one debt line, or None for any other line."""

    match = DEBT_LINE.match(line)
    return " ".join(match["debt"].split()) if match else None


def debts_at(revision: str) -> collections.Counter[str]:
    """Every debt a revision records (``:`` for the index), however recorded.

    A debt is a harness tag today; before the tags existed it was a
    ``backlog`` row of the registry file. Both count, so a comparison with an
    older base sees the same debts in their older form.
    """

    location = [] if revision == ":" else [revision]
    cached = ["--cached"] if revision == ":" else []
    listed = subprocess.run(
        ["git", "grep", "-h", "--no-color", "-I", *cached, "-e", f"@{DEBT_TAG}", *location, "--",
         ":(glob)crates/**/*.rs"],
        cwd=ROOT, capture_output=True, text=True, check=False,
    )
    if listed.returncode not in (0, 1):
        fail(f"cannot read the debts of {revision}: {listed.stderr.strip()}")
    found = collections.Counter(
        text for line in listed.stdout.splitlines() if (text := debt_text(line)) is not None
    )
    registry = subprocess.run(
        ["git", "show", f"{revision}{'' if revision == ':' else ':'}{REGISTRY.relative_to(ROOT).as_posix()}"],
        cwd=ROOT, capture_output=True, text=True, check=False,
    )
    if registry.returncode == 0:
        for row in json.loads(registry.stdout).get("backlog", ()):
            found[" ".join(value for value in (row["rule"], row["kernel"], row["parameter"]) if value)] += 1
    return found


def refuse_new_debt(before: collections.Counter[str], after: collections.Counter[str]) -> list[str]:
    """Debts ``after`` records that ``before`` did not: a move keeps its text."""

    return sorted((after - before).elements())


def ratchet_base(requested: str | None) -> str:
    """The merge-base the CI ratchet compares with, or a fail-closed refusal."""

    candidates = [requested] if requested else ["origin/main", "main"]
    for candidate in candidates:
        found = subprocess.run(
            ["git", "merge-base", "HEAD", candidate], cwd=ROOT, capture_output=True, text=True, check=False,
        )
        if found.returncode == 0 and found.stdout.strip():
            return found.stdout.strip()
    fail(
        f"cannot evaluate the @{DEBT_TAG} ratchet: HEAD has no merge-base with "
        f"{' or '.join(candidates)} (a shallow clone or a missing ref). Fetch full history "
        "(actions/checkout fetch-depth: 0) or pass --debt-ratchet <ref>; the ratchet is not skipped."
    )


def debt_ratchet(requested: str | None) -> None:
    base = ratchet_base(requested)
    before, after = debts_at(base), debts_at("HEAD")
    if sum(after.values()) > sum(before.values()):
        fail(
            f"HEAD records {sum(after.values())} @{DEBT_TAG} debts, more than the "
            f"{sum(before.values())} at its merge-base {base[:12]}; fix the harness instead: "
            + ", ".join(refuse_new_debt(before, after))
        )
    print(f"proof input debt: {sum(after.values())} at HEAD, {sum(before.values())} at merge-base {base[:12]}")


def report(found: list[dict[str, str]], complete: bool) -> None:
    justified, debts = load_registry()
    allowed = justified.keys() | debts.keys()
    seen = {key(finding) for finding in found}
    problems = []
    for finding in sorted(found, key=key):
        if key(finding) in allowed:
            continue
        problems.append(
            f"{finding['source']}::{finding['harness']} is a {finding['rule']} "
            + (
                f"over {finding['kernel']}({finding['parameter']})"
                if finding["parameter"]
                else f"in {finding['kernel']}"
            )
        )
    if complete:
        # Ratchet: a debt or entry that no longer matches a finding has been
        # fixed (or renamed) and must go, so the debt cannot be padded.
        for entry in sorted(debts.keys() - seen):
            problems.append(
                f"stale @{DEBT_TAG} at {debts[entry]['where']} matches no finding; delete that line"
            )
        for entry in sorted(justified.keys() - seen):
            problems.append(
                f"stale registry entry {entry[1]}::{entry[2]} ({entry[0]}) matches no finding"
            )
    if problems:
        joined = "\n  ".join(problems)
        fail(
            f"{len(problems)} proof(s) do not constrain what production derives:\n"
            f"  {joined}\n"
            "A harness must feed the production derivation symbolic fields, not a free "
            "symbol or the callee's own body. A genuinely environmental input belongs "
            "in formal/proof-input-exceptions.json under \"justified\" with a reason; "
            f"a new @{DEBT_TAG} line is refused."
        )
    print(
        f"proof input derivation: {len(found)} classified "
        f"({len(justified)} justified, {len(debts)} tagged @{DEBT_TAG})"
    )


def survey() -> None:
    found = findings()
    everything = rust_sources()
    sources = {
        path: _crate_boundary_workspace.rust_without_comments(
            path.read_text(errors="replace")
        )
        for path in everything
    }
    total = sum(len(list(harnesses(text))) for text in sources.values())
    flagged = {(finding["source"], finding["harness"]) for finding in found}
    by_rule: dict[str, int] = {}
    for finding in found:
        by_rule[finding["rule"]] = by_rule.get(finding["rule"], 0) + 1
    print(f"harnesses: {total}")
    print(f"(a) derived from real types: {total - len(flagged)}")
    print(f"(b)+(c) flagged harnesses: {len(flagged)}")
    for rule in RULES:
        print(f"    {rule}: {by_rule.get(rule, 0)} finding(s)")
    for finding in sorted(found, key=key):
        print(
            f"  {finding['rule']:<11} {finding['source']}::{finding['harness']}"
            f" [{finding['kernel']}{'/' + finding['parameter'] if finding['parameter'] else ''}]"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--require-complete", action="store_true")
    parser.add_argument("--survey", action="store_true")
    parser.add_argument("--staged", nargs="*", default=None)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument(
        "--debt-ratchet", nargs="?", const="", default=None, metavar="REF",
        help="refuse more debts at HEAD than at its merge-base with REF (default origin/main, then main)",
    )
    options = parser.parse_args()
    if options.selftest:
        selftest()
        return
    if options.debt_ratchet is not None:
        debt_ratchet(options.debt_ratchet or None)
    if options.survey:
        survey()
        return
    if options.staged is not None:
        paths = [ROOT / candidate for candidate in options.staged]
        interesting = [path for path in paths if path.suffix == ".rs"]
        registry_touched = any(
            candidate.endswith(("proof-input-exceptions.json", "formal/coverage.toml"))
            or candidate.startswith("formal/obligations/")
            for candidate in options.staged
        )
        head = subprocess.run(["git", "rev-parse", "--verify", "-q", "HEAD"], cwd=ROOT,
                              capture_output=True, check=False).returncode == 0
        new = refuse_new_debt(debts_at("HEAD") if head else collections.Counter(), debts_at(":"))
        if new:
            fail(
                f"the staged tree adds {len(new)} @{DEBT_TAG} debt(s) HEAD does not carry: "
                + ", ".join(new) + ". Fix the harness so it feeds the production derivation; "
                "a debt is only ever removed."
            )
        if not interesting and not registry_touched:
            return
        # A staged Rust file is judged on its own; a registry or ledger edit is
        # judged against the whole tree, because both are global statements.
        report(findings(None if registry_touched else interesting), registry_touched)
        return
    report(findings(), options.require_complete)


def selftest() -> None:
    """Decision table for the reader and all three rules.

    Rows: R1 a harness restating the callee's body (tautology); R2 the same
    kernel asserted against an independently stated specification (clean); R3 a
    free bool standing in for a fact production computes (free_symbol); R4 the
    same harness feeding the kernel the real derivation (clean); R5 a free
    symbol whose production counterpart merely forwards a stored value, which
    is a faithful over-approximation and not a finding; R6 a `#[cfg(kani)]`
    fork inside a function of a production entrypoint, and R7 the same file
    when no obligation names it.
    """

    kernel = "const fn kernel(a: bool, b: bool) -> bool { a && b }"
    derivation = "fn caller(x: &Thing) -> bool { kernel(x.p == x.q, x.n > 0) }"
    forwarding = "fn caller(x: &Thing) -> bool { kernel(x.flag, x.other) }"
    table = [
        (
            "R1 tautology",
            kernel + """
            #[kani::proof]
            fn harness() {
                let a: bool = kani::any();
                let b: bool = kani::any();
                assert_eq!(kernel(a, b), a && b);
            }
            """,
            {"tautology"},
        ),
        (
            "R2 independent specification",
            kernel + """
            #[kani::proof]
            fn harness() {
                let a: bool = kani::any();
                let b: bool = kani::any();
                assert_eq!(kernel(a, b), !(!a || !b));
            }
            """,
            set(),
        ),
        (
            "R3 free symbol for a computed fact",
            kernel + derivation + """
            #[kani::proof]
            fn harness() {
                let a: bool = kani::any();
                let b: bool = kani::any();
                assert!(kernel(a, b) || !a || !b);
            }
            """,
            {"free_symbol"},
        ),
        (
            "R4 the real derivation instead",
            kernel + derivation + """
            #[kani::proof]
            fn harness() {
                let n: u8 = kani::any();
                let thing = Thing { p: 1, q: n, n };
                assert!(caller(&thing) || n != 1 || n == 0);
            }
            """,
            set(),
        ),
        (
            "R5 forwarded value, not a computed fact",
            kernel + forwarding + """
            #[kani::proof]
            fn harness() {
                let a: bool = kani::any();
                let b: bool = kani::any();
                assert!(kernel(a, b) || !a || !b);
            }
            """,
            set(),
        ),
    ]
    for label, source, expected in table:
        text = _crate_boundary_workspace.rust_without_comments(source)
        path = ROOT / "crates" / "x" / "src" / "lib.rs"
        sources = {path: text}
        found = harness_findings(
            path, text, call_site_index(sources), definition_crates(sources)
        )
        assert {row["rule"] for row in found} == expected, (label, found)

    fork = _crate_boundary_workspace.rust_without_comments(
        """
        pub fn door(value: String) -> Self {
            #[cfg(kani)]
            { Self { value } }
            #[cfg(not(kani))]
            match Self::parse(value) { Ok(v) => v, Err(e) => panic!("{e}") }
        }
        """
    )
    path = ROOT / "crates" / "x" / "src" / "lib.rs"
    assert kani_fork_findings(path, fork, {"crates/x/src/lib.rs"}), "R6 fork in an entrypoint"
    assert not kani_fork_findings(path, fork, set()), "R7 no obligation names the file"

    # A registered exception is a written claim, not a mute button. R8..R11
    # check the registry's own shape rules against synthetic entries, so the
    # rules are exercised whether or not the checked-in registry violates them.
    row = {
        "rule": "free_symbol",
        "source": "crates/x/src/lib.rs",
        "harness": "h",
        "kernel": "k",
        "parameter": "p",
        "derived_in": "same_crate",
    }
    good = row | {"obligation": "o", "reason": "environmental: outside the repository"}
    validate_registry({"justified": [good]}, {"o"})  # R8 a complete entry is accepted
    for label, registry in (
        ("R9 justified without a reason", {"justified": [row | {"obligation": "o"}]}),
        ("R10 justified naming no obligation", {"justified": [good | {"obligation": "x"}]}),
        ("R11 a backlog section in the registry", {"justified": [good], "backlog": [row]}),
        ("R12 an unknown rule name", {"backlog": [row | {"rule": "invented"}]}),
    ):
        try:
            # `fail` narrates to stderr; a row that is *expected* to fail would
            # otherwise print an error beside a passing selftest.
            with contextlib.redirect_stderr(io.StringIO()):
                validate_registry(registry, {"o"})
        except SystemExit:
            continue
        raise AssertionError(label)

    # Debts. R13 a tag on a harness is a debt keyed by that harness; R14 a
    # malformed or repeated tag is refused by file:line; R15 a staged debt HEAD
    # does not carry is refused, a moved one (same text) is not; R16 a missing
    # merge-base fails closed instead of skipping the ratchet.
    harness = formal_anchors.Harness("h", "crates/x/src/lib.rs", 9, "x", [
        formal_anchors.Anchor("crates/x/src/lib.rs", 5, DEBT_TAG, "free_symbol k p"),
        formal_anchors.Anchor("crates/x/src/lib.rs", 6, DEBT_TAG, "tautology k"),
    ])
    debts, errors = debt_tags([harness])
    assert not errors and set(debts) == {
        ("free_symbol", "crates/x/src/lib.rs", "h", "k", "p"), ("tautology", "crates/x/src/lib.rs", "h", "k", ""),
    }, ("R13", debts, errors)
    for value in ("free_symbol k", "tautology k p", "invented k p"):
        broken = formal_anchors.Harness("h", "crates/x/src/lib.rs", 9, "x", [
            formal_anchors.Anchor("crates/x/src/lib.rs", 5, DEBT_TAG, value)])
        assert debt_tags([broken])[1][0].startswith("crates/x/src/lib.rs:5: expected"), ("R14", value)
    twice = formal_anchors.Harness("h", "crates/x/src/lib.rs", 9, "x", [harness.anchors[0], harness.anchors[0]])
    assert "repeats the debt" in debt_tags([twice])[1][0], "R14 repeated"
    assert debt_text("    /// @proof-input-debt  free_symbol k p ") == "free_symbol k p", "R15 text"
    assert debt_text("    /// @obligation a.b") is None, "R15 text"
    before = collections.Counter({"free_symbol k p": 1})
    assert refuse_new_debt(before, before) == [], "R15 a moved debt"
    assert refuse_new_debt(before, collections.Counter()) == [], "R15 a paid debt"
    assert refuse_new_debt(before, before + collections.Counter({"tautology k": 1})) == ["tautology k"], "R15"
    failed = subprocess.CompletedProcess([], 128, "", "fatal: not a valid object name")
    with unittest_mock.patch("subprocess.run", return_value=failed), contextlib.redirect_stderr(io.StringIO()):
        try:
            ratchet_base("missing")
        except SystemExit:
            pass
        else:
            raise AssertionError("R16 a missing merge-base must fail closed")

    print("proof input derivation: selftest passed")


if __name__ == "__main__":
    main()
