"""Session-acceptance evidence for the independent SDK consumer (T02-C01, T02-C03).

Run from the repository root:
    python -m unittest examples/sdk-consumer/tests/sdk_acceptance_test.py -v

| Row | Scenario | Cause | Effect |
|---|---|---|---|
| E1 | T02-C01 NC | real `cargo check` output of this consumer | `validate_task_evidence` (the gate's own validator) refuses it |
| E2 | T02-C01 | real `cargo test --test tasks` output | the same validator accepts it (E1 is not vacuous) |
| D1 | T02-C03 O0, NC | with E-code checking on (RUSTC_BOOTSTRAP=1): C3 copied verbatim from conversation.rs; C2/C3 bodies made to fail for an unrelated (unresolved path) reason; the C3 mutant without a pinned code | C3 passes; pinned mutants fail (code not produced); the unpinned mutant passes, so pinning is what detects them |
| D2 | T02-C03 | C2 copied verbatim, E-code checking on | its pinned code (E0425, ADR-0103) is the one rustc 1.96 reports |
| D3 | T02-C03 | D1's mutants on the pinned stable toolchain, in the environment the doctest gate uses | the gate checks E-codes (ADR-0103), so the pinned mutants fail |
| A1 | T02-C03 O0 | current awaken-runtime surface | equals public-api/awaken-runtime.txt |
| A2 | T02-C03 O0/O1, NC | a copy with one `pub use` removed | exactly that item is reported as removed drift |

Nothing under scripts/ci is modified; check_sdk_consumer.py is only imported.
"""
from __future__ import annotations

import difflib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[3]
CONSUMER = ROOT / "examples/sdk-consumer"
RUNTIME = ROOT / "crates/runtime/awaken-runtime"
CONVERSATION = RUNTIME / "src/conversation.rs"
SNAPSHOT = ROOT / "public-api/awaken-runtime.txt"
PUBLIC_API_NIGHTLY = "nightly-2026-07-14"
PUBLIC_API_TOOL_VERSION = "0.52.0"
OMIT = "blanket-impls,auto-trait-impls,auto-derived-impls"
REMOVED_REEXPORT = "pub use awaken_agent_contract::fresh_process_id;"
REMOVED_SNAPSHOT_LINE = "pub use awaken_runtime::fresh_process_id"

sys.path.insert(0, str(ROOT / "scripts/ci"))
import check_sdk_consumer  # noqa: E402  (imported, never edited)


def repository_toolchain() -> str:
    return tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]["channel"]


def environment(**overrides: str) -> dict[str, str]:
    env = dict(os.environ)
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.update(overrides)
    return env


def run(command: list[str], *, cwd: Path, env: dict[str, str] | None = None, timeout: int = 3000):
    return subprocess.run(command, cwd=cwd, env=env or environment(), capture_output=True,
                          text=True, encoding="utf-8", errors="replace", timeout=timeout)


class TaskEvidenceTests(unittest.TestCase):
    """T02-C01: a compile-only run cannot satisfy the consumer task evidence."""

    def test_e1_compile_only_output_is_refused_by_the_task_validator(self):
        checked = run(["cargo", "check", "--manifest-path", str(CONSUMER / "Cargo.toml"),
                       "--locked", "--tests"], cwd=ROOT)
        self.assertEqual(checked.returncode, 0, checked.stderr[-2000:])
        output = checked.stdout + checked.stderr
        self.assertIn("Finished", output)
        with self.assertRaisesRegex(RuntimeError, "task evidence is incomplete"):
            check_sdk_consumer.validate_task_evidence(output)

    def test_e2_executed_task_output_is_accepted_by_the_same_validator(self):
        tested = run(["cargo", "test", "--manifest-path", str(CONSUMER / "Cargo.toml"),
                      "--locked", "--test", "tasks"], cwd=ROOT)
        self.assertEqual(tested.returncode, 0, tested.stdout[-2000:] + tested.stderr[-2000:])
        check_sdk_consumer.validate_task_evidence(tested.stdout + tested.stderr)
        executed = re.findall(r"^test ([a-z_]+) \.\.\. ok$", tested.stdout, re.MULTILINE)
        self.assertEqual(set(executed), check_sdk_consumer.EXPECTED)


def doctest_block(source: str, fence: str) -> str:
    match = re.search(r"^/// ```" + re.escape(fence) + r"\n(.*?)^/// ```$", source, re.M | re.S)
    if match is None:
        raise AssertionError(f"no ```{fence} doctest in conversation.rs")
    return "".join(line.removeprefix("/// ").removeprefix("///") + "\n"
                   for line in match.group(1).splitlines())


def doc_item(name: str, fence: str, body: str) -> str:
    lines = [f"/// ```{fence}"] + [f"/// {line}".rstrip() for line in body.splitlines()] + ["/// ```"]
    return "\n".join(lines) + f"\npub fn {name}() {{}}\n"


def compile_fail_pin_results(**env: str) -> tuple[dict[str, str], dict[str, str], str]:
    """Run copies of conversation.rs's C2/C3 compile_fail doctests and their
    unrelated-failure mutants in an independent crate; return observed and
    correct per-item results and the raw output."""
    source = CONVERSATION.read_text(encoding="utf-8")
    c2 = doctest_block(source, "compile_fail,E0425")
    c3 = doctest_block(source, "compile_fail,E0599")
    assert "awaken_runtime::EmbeddedSession" in c2 and "use awaken_runtime::{" in c3
    c2_unrelated = c2.replace("awaken_runtime::EmbeddedSession", "awaken_runtime_absent::EmbeddedSession")
    c3_unrelated = c3.replace("use awaken_runtime::{", "use awaken_runtime_absent::{")
    assert c2_unrelated != c2 and c3_unrelated != c3
    items = {
        "c2_verbatim": ("compile_fail,E0425", c2, "ok"),
        "c3_verbatim": ("compile_fail,E0599", c3, "ok"),
        "c2_unrelated_import": ("compile_fail,E0425", c2_unrelated, "FAILED"),
        "c3_unrelated_import": ("compile_fail,E0599", c3_unrelated, "FAILED"),
        "c3_unrelated_import_unpinned": ("compile_fail", c3_unrelated, "ok"),
    }
    runtime_path = RUNTIME.as_posix()
    with tempfile.TemporaryDirectory() as directory:
        crate = Path(directory)
        (crate / "src").mkdir()
        (crate / "Cargo.toml").write_text(
            '[package]\nname = "compile-fail-pins"\nversion = "0.0.0"\nedition = "2024"\n'
            'publish = false\n\n[workspace]\n\n[dependencies]\n'
            f'awaken-runtime = {{ path = "{runtime_path}", default-features = false }}\n',
            encoding="utf-8")
        (crate / "src/lib.rs").write_text(
            "//! Copies of awaken-runtime's compile_fail doctests and their mutants.\n"
            + "".join(doc_item(name, fence, body) for name, (fence, body, _) in items.items()),
            encoding="utf-8")
        shutil.copy(CONSUMER / "Cargo.lock", crate / "Cargo.lock")
        tested = run(["cargo", "test", "--offline", "--doc", "--manifest-path", str(crate / "Cargo.toml")],
                     cwd=crate, env=environment(RUSTUP_TOOLCHAIN=repository_toolchain(), **env))
    output = tested.stdout + tested.stderr
    observed = dict(re.findall(
        r"^test src[\\/]lib\.rs - (\w+) \(line \d+\) - compile fail \.\.\. (ok|FAILED)$", output, re.M))
    return observed, {name: expected for name, (_, _, expected) in items.items()}, output


_PIN_RUNS: dict[tuple[tuple[str, str], ...], tuple[dict[str, str], dict[str, str], str]] = {}


def cached_pin_results(**env: str) -> tuple[dict[str, str], dict[str, str], str]:
    key = tuple(sorted(env.items()))
    if key not in _PIN_RUNS:
        _PIN_RUNS[key] = compile_fail_pin_results(**env)
    return _PIN_RUNS[key]


def doctest_gate_environment() -> dict[str, str]:
    """The environment assignments the workspace doctest step runs under."""
    gate = (ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
    match = re.search(r"\benv((?: [A-Z_]+=\S+)+) cargo test --workspace --doc\b", gate)
    if match is None:
        return {}
    return dict(assignment.split("=", 1) for assignment in match.group(1).split())


# rustdoc honours compile_fail E-codes only on a nightly-capable build;
# RUSTC_BOOTSTRAP=1 enables that on the repository's own 1.96 toolchain.
CODE_CHECKING = {"RUSTC_BOOTSTRAP": "1"}
MUTANT_ROWS = ("c2_unrelated_import", "c3_unrelated_import", "c3_unrelated_import_unpinned")


class CompileFailPinTests(unittest.TestCase):
    """T02-C03: a pinned E-code must detect a body that fails for another reason."""

    def test_d1_pinned_codes_detect_unrelated_import_mutants_with_code_checking(self):
        observed, correct, output = cached_pin_results(**CODE_CHECKING)
        rows = MUTANT_ROWS + ("c3_verbatim",)
        self.assertEqual({row: observed.get(row) for row in rows}, {row: correct[row] for row in rows},
                         output[-4000:])
        self.assertIn("E0433", output, "the mutants fail for the unrelated reason")
        self.assertIn('Some expected error codes were not found: ["E0599"]', output)

    def test_d2_reproducer_c2_doctest_pin_matches_the_compiler_error_code(self):
        observed, _, output = cached_pin_results(**CODE_CHECKING)
        self.assertEqual(observed.get("c2_verbatim"), "ok", output[-4000:])

    def test_d3_reproducer_stable_rustdoc_checks_pinned_codes(self):
        observed, correct, output = cached_pin_results(**doctest_gate_environment())
        self.assertEqual({row: observed.get(row) for row in MUTANT_ROWS},
                         {row: correct[row] for row in MUTANT_ROWS}, output[-4000:])


def public_api(workspace: Path) -> list[str]:
    computed = run(["cargo", f"+{PUBLIC_API_NIGHTLY}", "public-api", "-p", "awaken-runtime",
                    "--simplified", "--omit", OMIT], cwd=workspace,
                   env=environment(CARGO_TARGET_DIR=str(ROOT / f"target/public-api-{PUBLIC_API_NIGHTLY}"),
                                   CARGO_BUILD_BUILD_DIR=str(ROOT / f"target/public-api-{PUBLIC_API_NIGHTLY}")))
    if computed.returncode != 0:
        raise AssertionError(computed.stderr[-4000:])
    return computed.stdout.splitlines()


def drift(current: list[str]) -> tuple[list[str], list[str]]:
    snapshot = SNAPSHOT.read_text(encoding="utf-8").splitlines()
    diff = list(difflib.unified_diff(snapshot, current, lineterm="", n=0))
    removed = [line[1:] for line in diff if line.startswith("-") and not line.startswith("---")]
    added = [line[1:] for line in diff if line.startswith("+") and not line.startswith("+++")]
    return removed, added


class PublicApiSnapshotTests(unittest.TestCase):
    """T02-C03: the pinned snapshot tool reports a removed public capability."""

    def setUp(self):
        version = run(["cargo", "public-api", "--version"], cwd=ROOT)
        self.assertEqual(version.returncode, 0, "cargo-public-api is required, not optional")
        self.assertIn(PUBLIC_API_TOOL_VERSION, version.stdout)
        toolchains = run(["rustup", "toolchain", "list"], cwd=ROOT).stdout
        self.assertIn(PUBLIC_API_NIGHTLY, toolchains, "the pinned nightly is required")

    def test_a1_a2_removing_one_reexport_is_reported_as_drift(self):
        self.assertEqual(drift(public_api(ROOT)), ([], []), "A1 the current surface matches the snapshot")
        with tempfile.TemporaryDirectory() as directory:
            copy = Path(directory)
            for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
                shutil.copy(ROOT / name, copy / name)
            shutil.copytree(ROOT / "crates", copy / "crates",
                            ignore=shutil.ignore_patterns("target", "node_modules"))
            library = copy / "crates/runtime/awaken-runtime/src/lib.rs"
            text = library.read_text(encoding="utf-8")
            self.assertEqual(text.count(REMOVED_REEXPORT + "\n"), 1)
            library.write_text(text.replace(REMOVED_REEXPORT + "\n", ""), encoding="utf-8")
            removed, added = drift(public_api(copy))
        self.assertEqual(removed, [REMOVED_SNAPSHOT_LINE], "A2 exactly the removed capability")
        self.assertEqual(added, [])


if __name__ == "__main__":
    unittest.main()
