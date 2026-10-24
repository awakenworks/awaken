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

The corpus is *derived*, not listed. A hand-listed recovery path is a guard
narrower than its subject: the nine files named here when the rule was written
had already become thirty-one, and every file that had started reading
checkpoints in between -- `awaken-dispatch/src/durable.rs`,
`awaken-dispatch-execution/src/worker.rs`, `awaken-session-executor/src/host/session/run_ingress.rs`
among them -- was outside the scan while looking exactly like it was inside it.
So the recovery path is now *every production Rust module that names
`StreamCheckpoint` or `Observed`*: a file joins the rule by doing the thing the
rule is about.
"""

from __future__ import annotations

import argparse
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _crate_boundary_workspace  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]

# This checker walks the workspace's production Rust and its own module, so its
# lefthook trigger is rendered from exactly that set.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/src/**/*.rs",
    "crates/**/Cargo.toml",
    "scripts/ci/_crate_boundary_workspace.py",
)

# Where the tri-state itself is defined. R1 is enforced here.
OBSERVATION = "crates/contract/awaken-agent-contract/src/observation.rs"

# The port whose read must stay tri-state. R2 is enforced here.
PORT = "crates/contract/awaken-agent-contract/src/stream/checkpoint.rs"

# What puts a file on the recovery path: it names the observed value or the
# tri-state that carries it. R2 and R3 are enforced across every such file.
ON_THE_RECOVERY_PATH = re.compile(r"\bStreamCheckpoint\b|\bObserved\s*[<:]")

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

# Widening the corpus from nine hand-listed files to every file that reads a
# checkpoint surfaced one shape the nine never showed: the *dispatch transport*,
# which carries a checkpoint across a process boundary and whose error is a
# transport error, not an observation. It keeps `Result<Option<_>>` on purpose,
# and `FencedStreamCheckpointStore::get` turns it into the tri-state at exactly
# one place. That is a decision, so it is written down -- and checked: each entry
# names the file that performs the translation, and an entry whose translator
# stopped translating, or whose file no longer carries the shape, fails here.
TRANSLATED_TRANSPORT = {
    "crates/contract/awaken-dispatch-contract/src/dispatch/queue.rs": (
        "the DispatchQueue transport declares the remote checkpoint operations; its "
        "Err is 'the transport failed', which is ignorance, and the translation below "
        "is the only place that decides what that means"
    ),
    "crates/server/awaken-coordinator-client/src/dispatch_client.rs": (
        "the HTTP client implements that same transport declaration; its Err is an "
        "HTTP failure, which the single translation turns into Unknown(Storage)"
    ),
}
# The one place the transport's Result becomes an observation.
TRANSLATOR = "crates/shared/awaken-dispatch-execution/src/fenced_checkpoint.rs"
TRANSLATION = re.compile(
    r"load_stream_checkpoint[\s\S]{0,400}?Ok\(None\)\s*=>\s*Observed::Absent[\s\S]{0,200}?"
    r"Err\([^)]*\)\s*=>\s*Observed::Unknown"
)

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

    # The derivation cannot pass vacuously: the tri-state and its port are the
    # two files the rule is *about*, so their absence is a moved definition, not
    # a clean repository.
    missing = [path for path in (OBSERVATION, PORT) if path not in sources]
    if missing:
        errors.append(
            "the observation definition and its checkpoint port must be on the "
            "derived recovery path; they are not, so the type moved and the scan "
            "is looking at nothing: " + ", ".join(sorted(missing))
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

    # The registered transport declarations are only admitted while the single
    # translation that justifies them is still there, and while each registered
    # file still carries the shape it was registered for.
    translator = strip_rust(sources.get(TRANSLATOR, ""))
    if TRANSLATED_TRANSPORT and not TRANSLATION.search(translator):
        errors.append(
            f"{TRANSLATOR}: the dispatch transport is allowed to keep "
            "Result<Option<StreamCheckpoint>> only because this file turns it into "
            "Present/Absent/Unknown in one match. That translation is gone, so the "
            "registered transport declarations are now an unguarded fail-open."
        )
    for path in sorted(TRANSLATED_TRANSPORT):
        if path not in sources or not OLD_SHAPE.search(strip_rust(sources[path])):
            errors.append(
                f"{path} is registered as a transport declaration that keeps "
                "Result<Option<StreamCheckpoint>>, but no longer carries it; drop the "
                "stale entry so the exception does not outlive its reason"
            )

    for path in sorted(sources):
        text = strip_rust(sources[path])
        hit = OLD_SHAPE.search(text)
        if hit and path not in TRANSLATED_TRANSPORT:
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
    """The derived recovery path: production modules that name the observation.

    Reachability from the Cargo roots decides which modules are production, and
    the `cfg(test) == false` projection then blanks inline test items, so a
    `.unwrap()` written in a test fixture is not read as a collapsed recovery.
    """

    crates = root / "crates"
    # Candidates first, from the raw text: a file that never spells the value or
    # the tri-state cannot be on the path however it is projected. Module
    # reachability is then computed only for the crates those candidates live
    # in -- it is a per-crate walk from that crate's own Cargo roots, so the
    # answer for a candidate is the same either way, and the check stays fast
    # enough to run on every commit rather than only in the nightly gate.
    candidates: dict[Path, str] = {}
    for path in _crate_boundary_workspace.crate_source_files(crates):
        text = path.read_text(encoding="utf-8", errors="ignore")
        if ON_THE_RECOVERY_PATH.search(text):
            candidates[path] = text
    manifests = [
        manifest
        for manifest in _crate_boundary_workspace.crate_manifests(crates)
        if any(manifest.parent in path.parents for path in candidates)
    ]
    reachable = _crate_boundary_workspace.production_module_files(manifests)
    found: dict[str, str] = {}
    for path, text in candidates.items():
        if path.resolve() not in reachable:
            continue
        production = _crate_boundary_workspace.production_rust(text)
        if ON_THE_RECOVERY_PATH.search(production):
            found[path.relative_to(root).as_posix()] = production
    return found


def _probe_file(sources: dict[str, str]) -> str:
    """A consumer on the derived path, i.e. not the tri-state or its port."""

    return next(path for path in sorted(sources) if path not in (OBSERVATION, PORT))


class ObservationCollapseTests(unittest.TestCase):
    """Each rule must fail on a deliberately broken copy of the real corpus.

    Decision table, the derivation included:

    | Rule | Cause                                                     | Effect |
    |------|-----------------------------------------------------------|--------|
    | R0   | the repository as it stands                                | accept |
    | R0b  | an empty corpus (the definition moved)                     | reject |
    | R1   | any escape hatch added to the tri-state                    | reject |
    | R2   | the port reverted to `Result<Option<_>>`                   | reject |
    | R3   | a call site collapsing an observation                      | reject |
    | R3b  | the same text inside a comment or a string literal         | accept |
    | R4   | a file that names a checkpoint is on the derived path      | in scope |
    | R5   | a file that names neither is not                           | out of scope |
    | R6   | a test-only module naming one                              | out of scope |
    | R7   | a registered transport declaration keeping the old shape   | accept |
    | R8   | the same, once its single translation is gone              | reject |
    | R9   | a registered file that no longer carries the shape         | reject |
    | R10  | candidate text changes on a second acquisition             | first text only |
    | R11  | a second derivation would report a different corpus        | validated count |
    | R12  | a missing definition or translation in the derived corpus  | reject |
    | R13  | acquisition fails, or arguments are invalid                | fail before report |
    """

    # The derivation walks the whole workspace, so it runs once for the class
    # rather than once per probe; each probe copies the dict before breaking it.
    @classmethod
    def setUpClass(cls) -> None:
        cls.derived = current_sources(ROOT)

    def setUp(self) -> None:
        self.sources = dict(self.derived)

    def test_repository_is_clean(self) -> None:
        self.assertEqual(validation_errors(self.sources), [], "R0")

    def test_the_derivation_covers_what_the_hand_list_missed(self) -> None:
        # R4: membership is "this file reads a checkpoint", so the files that
        # started doing so after the hand-written tuple was frozen are in scope.
        for path in (
            "crates/runtime/awaken-runtime/src/engine/resume.rs",
            "crates/runtime/awaken-runtime/src/engine/inference.rs",
            "crates/contract/awaken-dispatch-contract/src/worker_transport.rs",
            "crates/server/awaken-coordinator-client/src/dispatch_client.rs",
        ):
            self.assertIn(path, self.sources, "R4")

    def test_the_derivation_excludes_what_does_not_read_a_checkpoint(self) -> None:
        # R5/R6: a file that names neither the value nor the tri-state is not on
        # the path, and neither is one that only names it from a test module.
        self.assertNotIn("crates/contract/awaken-agent-contract/src/lib.rs", self.sources, "R5")
        for path in self.sources:
            self.assertNotIn("/tests/", path, "R6")
        self.assertTrue(
            all(ON_THE_RECOVERY_PATH.search(text) for text in self.sources.values()), "R4"
        )

    def test_a_registered_transport_declaration_is_admitted(self) -> None:
        # R7: the registry is not empty and the repository passes with it, so the
        # exception is doing work rather than describing nothing.
        self.assertTrue(TRANSLATED_TRANSPORT, "R7")
        for path in TRANSLATED_TRANSPORT:
            self.assertIn(path, self.sources, "R7")
            self.assertTrue(OLD_SHAPE.search(strip_rust(self.sources[path])), "R7")

    def test_losing_the_single_translation_revokes_the_exception(self) -> None:
        # R8: the transport may keep `Result<Option<_>>` only while exactly one
        # place turns it into the tri-state. Remove that and the exception dies.
        broken = dict(self.sources)
        broken[TRANSLATOR] = broken[TRANSLATOR].replace(
            "Ok(None) => Observed::Absent,", "Ok(None) => Observed::Absent, // moved"
        ).replace("Err(error) => Observed::Unknown", "Err(_error) => panic!")
        self.assertTrue(validation_errors(broken), "R8")

    def test_a_registered_file_that_dropped_the_shape_fails(self) -> None:
        # R9: an exception that outlived its reason is itself a defect.
        path = sorted(TRANSLATED_TRANSPORT)[0]
        broken = dict(self.sources)
        broken[path] = broken[path].replace("Result<Option<StreamCheckpoint>", "Observed<StreamCheckpoint")
        self.assertTrue(validation_errors(broken), "R9")

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
            broken[_probe_file(self.sources)] = broken[_probe_file(self.sources)] + "\n" + injected
            self.assertTrue(validation_errors(broken), f"R3: {injected}")

    def test_a_collapse_inside_a_comment_or_string_is_not_a_violation(self) -> None:
        # R3: prose and SQL text are not code; the check must not false-positive.
        clean = dict(self.sources)
        clean[_probe_file(self.sources)] = (
            clean[_probe_file(self.sources)]
            + '\n// never write checkpoints.get(id).await.ok().flatten()\n'
            + 'const SQL: &str = "get(x).await.unwrap_or_default()";\n'
        )
        self.assertEqual(validation_errors(clean), [], "R3")

    def test_a_moved_recovery_file_fails_instead_of_passing_vacuously(self) -> None:
        # R0: an empty corpus must not look like a clean one.
        self.assertTrue(validation_errors({}), "R0")

    def test_derivation_reuses_its_candidate_text(self) -> None:
        # R10/R13 cause-effect table: marked+reachable -> the same acquired
        # text is projected; marked+test-only -> excluded (including ignored
        # invalid UTF-8); unmarked -> excluded. Module reachability has its own
        # required reads, counted separately. A changed second candidate read
        # must not remove a source selected from the first text.
        import tempfile
        from collections import Counter
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "crates/fixture"
            sources = crate / "src"
            sources.mkdir(parents=True)
            (crate / "Cargo.toml").write_text('[package]\nname = "fixture"\n')
            (sources / "lib.rs").write_text(
                'mod recovery;\n#[cfg(test)] mod test_only;\n'
            )
            candidate = sources / "recovery.rs"
            original = "fn read() { let _ = Observed::Absent; }\n"
            candidate.write_text(original)
            (sources / "test_only.rs").write_bytes(b"Observed::Absent;\xff\n")
            (sources / "unrelated.rs").write_text("fn unrelated() {}\n")
            reads, graph_reads = Counter(), Counter()
            read_text = Path.read_text
            module_files = _crate_boundary_workspace.production_module_files
            inside_graph = False

            def read(path, *args, **kwargs):
                counter = graph_reads if inside_graph else reads
                counter[path] += 1
                if path == candidate and not inside_graph and counter[path] > 1:
                    return "fn changed() {}\n"
                return read_text(path, *args, **kwargs)

            def graph(*args, **kwargs):
                nonlocal inside_graph
                inside_graph = True
                try:
                    return module_files(*args, **kwargs)
                finally:
                    inside_graph = False

            with patch.object(Path, "read_text", read), patch.object(
                _crate_boundary_workspace, "production_module_files", graph
            ):
                derived = current_sources(root)
            self.assertEqual(derived, {
                candidate.relative_to(root).as_posix():
                    _crate_boundary_workspace.production_rust(original)
            }, "R10 same candidate text")
            self.assertEqual(reads, Counter({path: 1 for path in sources.glob("*.rs")}), "R10 acquisition")
            self.assertEqual(graph_reads[candidate], 1, "R10 separate structural read")
            with patch.object(Path, "read_text", side_effect=PermissionError("unreadable")):
                with self.assertRaises(PermissionError):
                    current_sources(root)

    def test_main_reports_the_validated_derivation(self) -> None:
        # R11/R12: clean/missing-definition/missing-translation -> success with
        # that dict's size / original hard refusal. A different second result
        # must never supply the report. R13 invalid argv -> zero acquisition;
        # errors precede any success count. These are real rule evaluations.
        import contextlib
        import io
        from unittest.mock import patch

        missing_translation = dict(self.sources)
        missing_translation.pop(TRANSLATOR)
        for sources, expected in ((self.sources, 0), ({}, 1), (missing_translation, 1)):
            output, errors = io.StringIO(), io.StringIO()
            with patch(__name__ + ".current_sources", side_effect=[sources, {}]) as derive, \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                status = main([])
            self.assertEqual(status, expected, "R11/R12 original rule outcome")
            self.assertEqual(derive.call_count, 1, "R11 one derivation")
            if expected == 0:
                self.assertIn(f"{len(sources)} derived recovery-path files", output.getvalue(), "R11")
            else:
                self.assertEqual(output.getvalue(), "", "R12 no success report")
                self.assertIn("ERROR:", errors.getvalue(), "R12 hard refusal")
        with patch(__name__ + ".current_sources") as derive, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as refused:
                main(["--unknown"])
            self.assertEqual(refused.exception.code, 2, "R13 usage")
            derive.assert_not_called()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(
            ObservationCollapseTests
        )
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1

    sources = current_sources(args.root)
    errors = validation_errors(sources)
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
        print(
            "\nA failed or interrupted read is not evidence that nothing exists. "
            "Keep the read tri-state and handle Unknown explicitly.",
            file=sys.stderr,
        )
        return 1
    print(
        f"OK - {len(sources)} derived recovery-path files keep "
        "observations tri-state and uncollapsed."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
