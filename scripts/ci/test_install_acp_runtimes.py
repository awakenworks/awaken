"""Focused tests for the canonical Sandbox ACP runtime installer."""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


INSTALLER = Path(__file__).resolve().parents[2] / "deploy/images/sandbox/install-acp-runtimes.py"
SPEC = importlib.util.spec_from_file_location("install_acp_runtimes", INSTALLER)
assert SPEC is not None and SPEC.loader is not None
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class CodexPlatformInstallTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-codex-install-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.package = self.root / "@openai/codex/package.json"
        self.package.parent.mkdir(parents=True)
        self.package.write_text(
            json.dumps({
                "version": "0.153.3",
                "optionalDependencies": {
                    "@openai/codex-linux-x64": "npm:@openai/codex@0.153.3-linux-x64",
                },
            }),
            encoding="utf-8",
        )

    def _linux_x64(self):
        return patch.multiple(installer.platform, system=lambda: "Linux", machine=lambda: "x86_64")

    def test_missing_alias_installs_only_manifest_declared_version(self) -> None:
        # Causes: Codex selected, Linux/x64, CLI manifest declares an exact
        # platform alias, npm omitted that optional package. Effect: install
        # the declared alias once and require its directory. Rule P1.
        alias = self.root / "@openai/codex-linux-x64"

        def install(args, **kwargs):
            alias.mkdir()

        with self._linux_x64(), patch.object(installer.subprocess, "check_output", return_value=str(self.root)), \
                patch.object(installer.subprocess, "run", side_effect=install) as npm:
            installer.ensure_codex_platform_binary()
        npm.assert_called_once_with(
            ["npm", "install", "-g", "--no-audit", "--no-fund",
             "@openai/codex-linux-x64@npm:@openai/codex@0.153.3-linux-x64"],
            check=True,
        )

    def test_present_alias_needs_no_second_install(self) -> None:
        # Causes: P1's same pinned manifest and already present platform alias.
        # Effect: retain the accepted package without another npm effect. P2.
        (self.root / "@openai/codex-linux-x64").mkdir()
        with self._linux_x64(), patch.object(installer.subprocess, "check_output", return_value=str(self.root)), \
                patch.object(installer.subprocess, "run") as npm:
            installer.ensure_codex_platform_binary()
        npm.assert_not_called()

    def test_missing_or_changed_manifest_alias_fails_before_install(self) -> None:
        # Causes: Codex manifest omits the platform package or points to a
        # different version. Effect: fail closed before npm network/write. P3.
        for optional in ({}, {"@openai/codex-linux-x64": "npm:@openai/codex@0.1.0-linux-x64"}):
            with self.subTest(optional=optional):
                self.package.write_text(
                    json.dumps({"version": "0.153.3", "optionalDependencies": optional}),
                    encoding="utf-8",
                )
                with self._linux_x64(), patch.object(installer.subprocess, "check_output", return_value=str(self.root)), \
                        patch.object(installer.subprocess, "run") as npm:
                    with self.assertRaisesRegex(SystemExit, "does not declare the pinned"):
                        installer.ensure_codex_platform_binary()
                npm.assert_not_called()

    def test_install_without_package_is_rejected(self) -> None:
        # Causes: npm exits successfully but the platform package is still
        # absent. Effect: image build fails before runtime admission. P4.
        with self._linux_x64(), patch.object(installer.subprocess, "check_output", return_value=str(self.root)), \
                patch.object(installer.subprocess, "run"):
            with self.assertRaisesRegex(SystemExit, "was not installed"):
                installer.ensure_codex_platform_binary()

    def test_unsupported_target_is_rejected_before_npm(self) -> None:
        # Causes: target OS/architecture is outside the declared Linux image
        # contract. Effect: fail without reading or installing a wrong alias. P5.
        with patch.multiple(installer.platform, system=lambda: "Linux", machine=lambda: "riscv64"), \
                patch.object(installer.subprocess, "check_output") as npm:
            with self.assertRaisesRegex(SystemExit, "supported Linux architecture"):
                installer.ensure_codex_platform_binary()
        npm.assert_not_called()

    def test_non_codex_selection_never_reads_codex_package(self) -> None:
        # Causes: another ACP runtime alone is selected. Effect: the existing
        # general npm installer runs, while Codex-specific repair stays idle. P6.
        contract = self.root / "contract.json"
        contract.write_text(json.dumps({"runtimes": [{
            "id": "claude", "requirements": [{"manager": "npm", "requirement": "example@1"}],
            "executables": ["example"],
        }]}), encoding="utf-8")
        with patch.object(installer.sys, "argv", ["install-acp-runtimes.py", str(contract), "claude"]), \
                patch.object(installer.subprocess, "run") as npm, \
                patch.object(installer, "ensure_codex_platform_binary") as codex, \
                patch.object(installer.shutil, "which", return_value="/usr/bin/example"):
            self.assertEqual(installer.main(), 0)
        npm.assert_called_once_with(
            ["npm", "install", "-g", "--no-audit", "--no-fund", "example@1"], check=True,
        )
        codex.assert_not_called()


if __name__ == "__main__":
    unittest.main()
