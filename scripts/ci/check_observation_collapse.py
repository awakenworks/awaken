#!/usr/bin/env python3
"""Architecture fitness function: a failed read may never become an absence.

Usage: check_observation_collapse.py [--root DIR] [--self-test]

The recurring defect this guards is *fail-open recovery*: an interrupted or
failed read of durable state is treated as "nothing is there", and the runtime
then starts fresh execution over state that actually exists.

`Observed<T>` (``crates/contract/awaken-agent-contract/src/observation.rs``)
removes the mistake from the type system by having three outcomes and no
combinator that fuses two of them. That only holds while two things stay true,
and those are what this check enforces:

R1  The tri-state keeps **no escape hatch**. The moment someone adds
    ``Observed::ok()``, a ``From<Observed<T>> for Option<T>``, or a ``Default``,
    every call site can collapse `Unknown` into `Absent` again with one token
    and the type guarantee is gone.

R2  The checkpoint read port returns the tri-state, not ``Result<Option<_>>``,
    and no recovery-path file re-declares the old shape. ``Result<Option<T>, E>``
    is the shape that invites ``.ok().flatten()``.

R3  No recovery-path file collapses an observation at the call site: an awaited
    checkpoint read, or an `Observed` expression, is consumed by `match`, never
    by ``.ok()`` / ``.unwrap_or_default()`` / ``.flatten()`` and friends.

The allow-list below is deliberately short and named file by file: it is every
file that produces or consumes a checkpoint observation. A file that starts
reading checkpoints belongs in it.
"""

from __future__ import annotations

import argparse
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Where the tri-state itself is defined. R1 is enforced here.
OBSERVATION = "crates/contract/awaken-agent-contract/src/observation.rs"

# The port whose read must stay tri-state. R2 is enforced here.
PORT = "crates/contract/awaken-agent-contract/src/stream/checkpoint.rs"

# Every file that produces or consumes a checkpoint observation: the port, its
# backends, the adapters between them, and the runtime that acts on the answer.
# R2 and R3 are enforced across all of them.
RECOVERY_PATH = (
    OBSERVATION,
    PORT,
    "crates/runtime/awaken-runtime/src/engine/model_operation.rs",
    "crates/runtime/awaken-runtime/src/engine/run_loop.rs",
    "crates/runtime/awaken-store-inmem/src/lib.rs",
    "crates/stores/awaken-store-fs/src/lib.rs",
    "crates/server/awaken-dispatch/src/fenced_checkpoint.rs",
    "crates/server/awaken-dispatch/src/postgres_checkpoint.rs",
    "crates/server/awaken-dispatch-http/src/worker_dispatch.rs",
)

# Methods that turn three outcomes into two. `Observed` defines none of them, so
# a hit is either a newly added escape hatch or a revert to `Result<Option<_>>`.
COLLAPSING = frozenset(
    {
        "ok",
        "err",
        "ok_or",
        "ok_or_else",
        "flatten",
        "unwrap",
        "unwrap_or",
        "unwrap_or_default",
        "unwrap_or_else",
        "expect",
        "into_option",
        "is_none",
        "is_none_or",
        "is_some",
        "unwrap_or_insert",
    }
)

# Escape hatches on the tri-state itself, as they would appear in its module.
ESCAPE_HATCHES = (
    (re.compile(r"\bfn\s+(?:ok|err|flatten|unwrap\w*|into_option|into_inner)\b"),
     "Observed must expose no combinator that drops a variant"),
    (re.compile(r"\bfor\s+Option\s*<"),
     "Observed must not convert into Option: that erases Unknown"),
    (re.compile(r"\bimpl\b[^\n]*\bDefault\b[^\n]*\bfor\s+Observed\b"),
     "Observed must have no Default: an unread state has no neutral value"),
    (re.compile(r"#\[derive\([^)]*\bDefault\b[^)]*\)\]\s*(?:#\[[^\]]*\]\s*)*"
                r"(?:pub\s+)?enum\s+Observed\b"),
     "Observed must not derive Default: an unread state has no neutral value"),
)

# The old shape, in any declaration on the recovery path.
OLD_SHAPE = re.compile(r"Result\s*<\s*Option\s*<\s*StreamCheckpoint\b")

# The declaration the port must keep.
TRI_STATE_READ = re.compile(
    r"async\s+fn\s+get\s*\([^)]*\)\s*->\s*Observed\s*<\s*StreamCheckpoint\s*>"
)


def strip_rust(source: str) -> str:
    """Blank out comments and string/char literals, preserving offsets.

    Chain scanning must not trip over a `.ok()` inside a doc comment or a SQL
    string. Replacing with spaces rather than deleting keeps every reported
    offset usable against the original text.
    """
    out = list(source)
    index = 0
    length = len(source)
    while index < length:
        two = source[index : index + 2]
        if two == "//":
            end = source.find("\n", index)
            end = length if end < 0 else end
            out[index:end] = " " * (end - index)
            index = end
        elif two == "/*":
            end = source.find("*/", index + 2)
            end = length if end < 0 else end + 2
            out[index:end] = " " * (end - index)
            index = end
        elif source[index] == '"':
            cursor = index + 1
            while cursor < length and source[cursor] != '"':
                cursor += 2 if source[cursor] == "\\" else 1
            cursor = min(cursor + 1, length)
            out[index:cursor] = " " * (cursor - index)
            index = cursor
        else:
            index += 1
    return "".join(out)


def skip_balanced(source: str, index: int, opening: str, closing: str) -> int:
    """Index just past the group that starts at `index`, or `index` if none."""
    if index >= len(source) or source[index] != opening:
        return index
    depth = 0
    while index < len(source):
        if source[index] == opening:
            depth += 1
        elif source[index] == closing:
            depth -= 1
            if depth == 0:
                return index + 1
        index += 1
    return index


IDENTIFIER = re.compile(r"[A-Za-z_]\w*")


def method_chain(source: str, index: int) -> list[str]:
    """Method/field names applied to the expression ending at `index`.

    Walks `.name(...)`, `.name`, `.await`, `?` and turbofish, so the chain of an
    expression is available as plain names regardless of line breaks.
    """
    names: list[str] = []
    length = len(source)
    while index < length:
        while index < length and source[index].isspace():
            index += 1
        if index < length and source[index] == "?":
            index += 1
            continue
        if index >= length or source[index] != ".":
            return names
        index += 1
        while index < length and source[index].isspace():
            index += 1
        match = IDENTIFIER.match(source, index)
        if not match:
            return names
        names.append(match.group(0))
        index = match.end()
        if source[index : index + 2] == "::":
            index = skip_balanced(source, source.find("<", index), "<", ">")
        index = skip_balanced(source, index, "(", ")")
    return names


AWAITED_READ = re.compile(r"\bget\s*\(")
OBSERVED_EXPRESSION = re.compile(r"\bObserved\s*::\s*\w+")


def observation_chains(source: str) -> list[tuple[int, list[str]]]:
    """(offset, chain) for every expression that yields an `Observed`.

    Two producers are recognised: an awaited `get(...)` — the shape of every
    checkpoint-store read — and a literal `Observed::` expression.
    """
    found: list[tuple[int, list[str]]] = []
    for match in AWAITED_READ.finditer(source):
        chain = method_chain(source, skip_balanced(source, match.end() - 1, "(", ")"))
        if chain[:1] == ["await"]:
            found.append((match.start(), chain[1:]))
    for match in OBSERVED_EXPRESSION.finditer(source):
        end = skip_balanced(source, match.end(), "(", ")")
        found.append((match.start(), method_chain(source, end)))
    return found


def line_of(source: str, offset: int) -> int:
    return source.count("\n", 0, offset) + 1


def validation_errors(sources: dict[str, str]) -> list[str]:
    """Every rule violation in the given `path -> text` recovery-path corpus."""
    errors: list[str] = []

    missing = [path for path in RECOVERY_PATH if path not in sources]
    if missing:
        errors.append(
            "recovery-path files are missing; update RECOVERY_PATH when code "
            "moves: " + ", ".join(sorted(missing))
        )

    observation = strip_rust(sources.get(OBSERVATION, ""))
    for pattern, reason in ESCAPE_HATCHES:
        hit = pattern.search(observation)
        if hit:
            errors.append(
                f"{OBSERVATION}:{line_of(observation, hit.start())}: {reason} "
                f"(found {hit.group(0).strip()!r}). The only way out of Unknown "
                "is an explicit match."
            )

    port = strip_rust(sources.get(PORT, ""))
    if port and not TRI_STATE_READ.search(port):
        errors.append(
            f"{PORT}: the checkpoint read must be declared "
            "`async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint>`; "
            "a Result/Option read lets a caller manufacture absence."
        )

    for path in RECOVERY_PATH:
        text = strip_rust(sources.get(path, ""))
        hit = OLD_SHAPE.search(text)
        if hit:
            errors.append(
                f"{path}:{line_of(text, hit.start())}: a checkpoint read may not "
                "return Result<Option<StreamCheckpoint>, _>; return "
                "Observed<StreamCheckpoint> so `.ok().flatten()` cannot be written."
            )
        for offset, chain in observation_chains(text):
            collapsed = [name for name in chain if name in COLLAPSING]
            if collapsed:
                errors.append(
                    f"{path}:{line_of(text, offset)}: an observation is collapsed "
                    f"by .{collapsed[0]}(); consume it with a match that names "
                    "Present, Absent and Unknown separately."
                )
    return errors


def current_sources(root: Path) -> dict[str, str]:
    return {
        path: (root / path).read_text(encoding="utf-8")
        for path in RECOVERY_PATH
        if (root / path).is_file()
    }


class ObservationCollapseTests(unittest.TestCase):
    """Each rule must fail on a deliberately broken copy of the real corpus."""

    def setUp(self) -> None:
        self.sources = current_sources(ROOT)

    def test_repository_is_clean(self) -> None:
        self.assertEqual(validation_errors(self.sources), [], "R0")

    def test_escape_hatch_on_the_tri_state_fails(self) -> None:
        # R1: adding any way out of Unknown other than `match` is rejected.
        for injected in (
            "impl<T> Observed<T> { pub fn ok(self) -> Option<T> { None } }",
            "impl<T> From<Observed<T>> for Option<T> { }",
            "impl<T> Default for Observed<T> { }",
            "pub fn unwrap_or_default(self) -> T { }",
        ):
            broken = dict(self.sources)
            broken[OBSERVATION] = broken[OBSERVATION] + "\n" + injected
            self.assertTrue(validation_errors(broken), f"R1: {injected}")

    def test_derived_default_on_the_tri_state_fails(self) -> None:
        # R1: `Default` would make "unread" indistinguishable from a value.
        broken = dict(self.sources)
        broken[OBSERVATION] = broken[OBSERVATION].replace(
            "#[derive(Debug, Clone, PartialEq, Eq)]\n#[must_use",
            "#[derive(Debug, Clone, Default, PartialEq, Eq)]\n#[must_use",
        )
        self.assertTrue(validation_errors(broken), "R1")

    def test_reverting_the_port_to_result_option_fails(self) -> None:
        # R2: the shape that invites `.ok().flatten()` must not come back.
        broken = dict(self.sources)
        broken[PORT] = broken[PORT].replace(
            "async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint>;",
            "async fn get(&self, run_id: &str) "
            "-> Result<Option<StreamCheckpoint>, StreamCheckpointError>;",
        )
        self.assertTrue(validation_errors(broken), "R2")

    def test_collapsing_a_read_at_the_call_site_fails(self) -> None:
        # R3: a caller may not fuse Unknown into Absent with a combinator.
        for injected in (
            "let partial = checkpoints.get(run_id).await.ok().flatten();",
            "let partial = checkpoints.get(run_id).await.unwrap_or_default();",
            "let partial = Observed::Absent.unwrap_or_else(|| fresh());",
        ):
            broken = dict(self.sources)
            broken[RECOVERY_PATH[2]] = broken[RECOVERY_PATH[2]] + "\n" + injected
            self.assertTrue(validation_errors(broken), f"R3: {injected}")

    def test_a_collapse_inside_a_comment_or_string_is_not_a_violation(self) -> None:
        # R3: prose and SQL text are not code; the check must not false-positive.
        clean = dict(self.sources)
        clean[RECOVERY_PATH[2]] = (
            clean[RECOVERY_PATH[2]]
            + '\n// never write checkpoints.get(id).await.ok().flatten()\n'
            + 'const SQL: &str = "get(x).await.unwrap_or_default()";\n'
        )
        self.assertEqual(validation_errors(clean), [], "R3")

    def test_a_moved_recovery_file_fails_instead_of_passing_vacuously(self) -> None:
        # R0: an empty corpus must not look like a clean one.
        self.assertTrue(validation_errors({}), "R0")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(
            ObservationCollapseTests
        )
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1

    errors = validation_errors(current_sources(args.root))
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
        print(
            "\nA failed or interrupted read is not evidence that nothing exists. "
            "Keep the read tri-state and handle Unknown explicitly.",
            file=sys.stderr,
        )
        return 1
    print(
        f"OK - {len(RECOVERY_PATH)} recovery-path files keep observations "
        "tri-state and uncollapsed."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
