#!/usr/bin/env python3
"""Refuse a trait default that silently degrades to a less-qualified sibling.

One shape produced the largest share of this repository's recurring defects.
A trait declares a qualified operation and gives it a default body that drops
the qualifier and calls the unqualified sibling:

    async fn create_as(&self, store, path, content, actor: Option<&MemoryActor>)
        -> Result<Memory, MemErr>
    {
        let _ = actor;                       // the qualifier is discarded
        self.create(store, path, content).await
    }

An implementation that does not override it satisfies the trait while providing
none of what the qualifier names, and the caller cannot tell. The qualifier has
been the tenant, the effect fence, the operation identity, the claim, the
attribution and the observation — which is why fixes for these read as six
unrelated families instead of one shape.

The rule is not "no defaults". A default that fails closed is fine: an
implementation that forgets it gets an error, not a quiet downgrade. What is
refused is a default that *succeeds* by calling a weaker sibling.

Whole-repo scan plus an exemption registry, never a file allow-list: a list
freezes today's instances, a scan also refuses tomorrow's.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Key: "<trait>::<method>". Value: why the degraded call is the correct
# behaviour for an implementation that does not override it. An entry is a
# decision someone made and can be argued with.
DEGRADING_DEFAULT_EXEMPTIONS: dict[str, str] = {}

# The shape predates the gate. Converting every instance at once would be a
# migration, not a fix, so the untriaged remainder is ratcheted instead:
# the count may fall and may not rise. That is what makes this gate prevent
# *new* instances today rather than only after the migration finishes — the
# failure mode of a file allow-list, which freezes the debt and guards nothing.
# Lower this number as instances are converted or registered; never raise it.
UNTRIAGED_BASELINE = 31

TRAIT = re.compile(r"pub trait (\w+)[^{;]*\{")
METHOD = re.compile(r"(?:async )?fn ([a-z_]\w*)\((?:[^()]|\([^()]*\))*\)\s*->\s*[^;{]*\{")


def _brace_span(text: str, open_index: int) -> int:
    depth = 0
    index = open_index
    while index < len(text):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return len(text) - 1


def degrading_default_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    seen: set[str] = set()
    for relpath, text in sorted(sources.items()):
        for trait_match in TRAIT.finditer(text):
            open_index = text.index("{", trait_match.start())
            body = text[open_index : _brace_span(text, open_index) + 1]
            siblings = {m.group(1) for m in re.finditer(r"fn ([a-z_]\w*)\(", body)}
            for method in METHOD.finditer(body):
                name = method.group(1)
                impl = body[method.end() - 1 : _brace_span(body, method.end() - 1) + 1]
                if "Err(" in impl:
                    continue  # a default that fails closed is not a downgrade
                # Two independent signals, either of which means the default
                # provides less than the signature promises:
                #   (a) a parameter is explicitly thrown away (`let _ = x;`)
                #       and the body then delegates — the qualifier is named in
                #       the signature and discarded in the body;
                #   (b) the delegate's name is the qualified name's stem, so the
                #       default is the unqualified operation under a qualified
                #       name. (a) is the precise one and catches an instance
                #       whose names are unrelated; (b) catches a drop that is
                #       implicit rather than spelled `let _ =`.
                discards = bool(re.search(r"let\s+_\s*(?::[^=]+)?=\s*\w+\s*;", impl))
                for sibling in siblings:
                    if sibling == name or not re.search(rf"self\.{sibling}\(", impl):
                        continue
                    stem = name.split("_for_")[0]
                    related = name.startswith(sibling) or sibling.startswith(stem)
                    if not (discards or related):
                        continue
                    key = f"{trait_match.group(1)}::{name}"
                    seen.add(key)
                    if key in DEGRADING_DEFAULT_EXEMPTIONS:
                        break
                    line = text[: open_index + method.start()].count("\n") + 1
                    errors.append(
                        f"{relpath}:{line}: {key} defaults to self.{sibling}(), dropping what "
                        f"{name!r} names; an implementation that omits it satisfies the trait "
                        "while providing none of the qualifier. Require the method (no default), "
                        "make the default fail closed, or record it in "
                        "DEGRADING_DEFAULT_EXEMPTIONS"
                    )
                    break
    for key in sorted(DEGRADING_DEFAULT_EXEMPTIONS):
        if key not in seen:
            errors.append(
                f"DEGRADING_DEFAULT_EXEMPTIONS names {key}, which no longer degrades; "
                "drop the stale exemption"
            )
    return errors


def _sources(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8", errors="ignore")
        for path in sorted((root / "crates").rglob("*.rs"))
        if not path.name.endswith(("tests.rs", "test_support.rs")) and "/tests/" not in str(path)
    }


def selftest() -> None:
    # Causes: C1 default delegates to a weaker sibling; C2 default fails closed;
    # C3 no default at all; C4 the degrade is registered; C5 a registry entry
    # names a method that no longer degrades.
    # Effects: E1 admitted, E2 rejected.
    # R1 = C1 => E2.  R2 = C2|C3|C4 => E1.  R3 = C5 => E2.
    global DEGRADING_DEFAULT_EXEMPTIONS
    DEGRADING_DEFAULT_EXEMPTIONS = {}

    degrading = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        let _ = actor;\n"
        "        self.write(p).await\n"
        "    }\n"
        "}\n"
    )}
    assert degrading_default_violations(degrading), "C1/E2"

    fail_closed = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        Err(Error::new(\"unimplemented\"))\n"
        "    }\n"
        "}\n"
    )}
    assert not degrading_default_violations(fail_closed), "C2/E1"

    required = {"a.rs": "pub trait Repo {\n    async fn write_as(&self, p: &str, actor: A) -> R;\n}\n"}
    assert not degrading_default_violations(required), "C3/E1"

    DEGRADING_DEFAULT_EXEMPTIONS = {"Repo::write_as": "test"}
    assert not degrading_default_violations(degrading), "C4/E1"
    assert degrading_default_violations(required), "C5/E2"
    DEGRADING_DEFAULT_EXEMPTIONS = {}
    print("check_degrading_defaults selftest: the decision table holds")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        selftest()
        raise SystemExit(0)
    problems = degrading_default_violations(_sources(ROOT))
    count = len(problems)
    if count > UNTRIAGED_BASELINE:
        for problem in problems:
            print(f"ERROR: {problem}", file=sys.stderr)
        print(
            f"\n{count} degrading defaults, baseline {UNTRIAGED_BASELINE}: "
            f"{count - UNTRIAGED_BASELINE} new one(s). Require the method, make the "
            "default fail closed, or register the decision.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    if count < UNTRIAGED_BASELINE:
        print(
            f"{count} degrading defaults, below the baseline of {UNTRIAGED_BASELINE}. "
            f"Lower UNTRIAGED_BASELINE to {count} so the ground gained is held.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print(f"OK - degrading defaults held at the baseline of {UNTRIAGED_BASELINE}.")
