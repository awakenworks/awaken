#!/usr/bin/env python3
"""Execute the real portability selector with inert substrate/test commands."""

from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib
import unittest

from _native_shell import bash_executable

ROOT = Path(__file__).resolve().parents[2]
GATE = "scripts/ci/check-trusted-host-portability.sh"
PREFIX = "host::tests::resource_activation::"
PUBLICATION = (
    PREFIX + "managed_terminal_repository_publication_pushes_exact_commit_and_replays",
    PREFIX + "terminal_repository_publication_closed_source_converges_without_git",
)


def select(system: str, arguments: tuple[str, ...]) -> tuple[int, list[list[str]]]:
    # Only composition is under test. The real libtest evidence owner separately
    # rejects missing required cases, unavailable fixtures and skipped assertions.
    bash = bash_executable()
    with tempfile.TemporaryDirectory(prefix="awaken-portability-selection-") as raw:
        root = Path(raw)
        ci = root / "scripts/ci"
        ci.mkdir(parents=True)
        (root / GATE).write_bytes((ROOT / GATE).read_bytes())
        (ci / "_cargo_target.sh").write_text("""awaken_configure_cargo_target() { :; }
awaken_unset_ambient_api_keys() { :; }
awaken_append_repo_tools() { :; }
uname() { printf '%s\n' "$TEST_SYSTEM"; }
cargo() { :; }
node() { printf '%s\n' test-owned-helper; }
python3() { :; }
cygpath() { printf '%s\n' "$2"; }
bwrap() { :; }
run_required_rust_tests() {
  printf '%s\t' "$@" >> selected.tsv
  printf '\n' >> selected.tsv
}
""", encoding="utf-8")
        for name in ("_provider_environment.sh", "_rust_test_execution.sh"):
            (ci / name).write_text("# Test-owned inert dependencies.\n", encoding="utf-8")
        result = subprocess.run(
            [bash, GATE, *arguments], cwd=root,
            env=dict(os.environ, TEST_SYSTEM=system),
            capture_output=True, text=True, timeout=30,
        )
        recorded = root / "selected.tsv"
        calls = [line.rstrip("\t").split("\t") for line in recorded.read_text().splitlines()] if recorded.exists() else []
        return result.returncode, calls


class PortabilitySelectionTests(unittest.TestCase):
    def assert_realization_owner_plan(self, calls: list[list[str]]) -> None:
        # The source owner supplies the package and prompt identities. A crate
        # extraction must fail this cheap matrix before a long native build can
        # discover an empty selection. The required runner checks live outcomes.
        realization = ROOT / "crates/server/awaken-session-realization"
        source = realization / "src/managed_input_layout/resource_prompt_tests.rs"
        package = tomllib.loads((realization / "Cargo.toml").read_text(encoding="utf-8"))["package"]["name"]
        prompt = [call for call in calls if call[3] == source.stem]
        self.assertEqual(len(prompt), 1)
        self.assertEqual(prompt[0][:3], ["-p", package, "--lib"])
        names = re.findall(r"^fn (resource_prompt_\w+)\(", source.read_text(encoding="utf-8"), re.MULTILINE)
        self.assertTrue(names, "the prompt decision tables must exist")
        owner = source.with_name("resource_prompt.rs")
        self.assertIn('#[path = "resource_prompt_tests.rs"]\nmod resource_prompt_tests;',
                      owner.read_text(encoding="utf-8"))
        module = "::".join(owner.relative_to(realization / "src").with_suffix("").parts) + "::" + source.stem
        self.assertCountEqual(prompt[0][4:], ["--require-case=" + module + "::" + name for name in names])

        host = ROOT / "crates/server/awaken-session-executor"
        composed = host / "src/realization_composed_tests/session_environment.rs"
        self.assertIn('mod tests;', composed.read_text(encoding="utf-8"))
        fixture = host / "src/session_environment/tests/shared_environment.rs"
        name = "native_process_and_agent_channel_share_one_live_environment"
        self.assertIn("async fn " + name + "(", fixture.read_text(encoding="utf-8"))
        module = "::".join(composed.relative_to(host / "src").with_suffix("").parts)
        identity = module + "::tests::shared_environment::" + name
        environment = [call for call in calls if call[3].endswith("::" + name)]
        self.assertEqual(environment, [["-p", "awaken-session-executor", "--lib", identity,
                                       "--require-case=" + identity, "--", "--exact"]])

    def assert_terminal_plan(self, calls: list[list[str]], supervision: bool) -> None:
        host_calls = [call for call in calls if call[:3] == ["-p", "awaken-session-executor", "--lib"]]
        portable = [call for call in host_calls if call[3] == "terminal_"]
        self.assertEqual(len(portable), 1, "portable terminal cases must execute once")
        self.assertEqual(portable[0][4:], ["--", "--skip", PUBLICATION[0], "--skip", PUBLICATION[1]])
        namespace = [call for call in host_calls if call[3] == "terminal_repository_publication"]
        self.assertEqual(len(namespace), int(supervision))
        if supervision:
            self.assertEqual(namespace[0][4:], [*("--require-case=" + name for name in PUBLICATION), "--", "--ignored"])

    def test_default_os_rows_only_select_portable_terminal_behavior(self) -> None:
        # P1 Windows, P2 macOS, P3 default Linux: no Namespace claim.
        for system in ("MINGW64_NT-10.0", "Darwin", "Linux"):
            with self.subTest(system=system):
                code, calls = select(system, ())
                self.assertEqual(code, 0)
                self.assert_terminal_plan(calls, False)
                self.assert_realization_owner_plan(calls)

    def test_linux_supervision_requires_both_real_publication_cases(self) -> None:
        # P4 Linux plus explicit substrate gate: both names and ignored mode.
        code, calls = select("Linux", ("--linux-supervision",))
        self.assertEqual(code, 0)
        self.assert_terminal_plan(calls, True)
        self.assert_realization_owner_plan(calls)

    def test_non_linux_supervision_fails_before_any_test_selection(self) -> None:
        # P5/P6 unsupported OS: fail before even the portable run starts.
        for system in ("MINGW64_NT-10.0", "Darwin"):
            with self.subTest(system=system):
                code, calls = select(system, ("--linux-supervision",))
                self.assertEqual(code, 2)
                self.assertEqual(calls, [])

    def test_invalid_or_duplicate_options_fail_before_selection(self) -> None:
        # P7/P8 unknown or repeated option cannot silently weaken the matrix.
        for arguments in (("--unknown",), ("--linux-supervision", "--linux-supervision")):
            with self.subTest(arguments=arguments):
                code, calls = select("Linux", arguments)
                self.assertEqual(code, 2)
                self.assertEqual(calls, [])


def self_test() -> None:
    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(PortabilitySelectionTests))
    if not result.wasSuccessful():
        raise AssertionError("TrustedHost portability selection matrix failed")


if __name__ == "__main__":
    unittest.main()
