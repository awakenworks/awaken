#!/usr/bin/env python3
"""Validate the one tag-driven Awaken product release graph."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from _native_shell import bash_executable
from _native_test_launcher import python_script_header


REPOSITORY = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY / ".github/workflows/release.yml"
MANAGEMENT_DOCKERFILE = REPOSITORY / "deploy/images/management/Dockerfile"
MANAGEMENT_DOCKERIGNORE = REPOSITORY / "deploy/images/management/Dockerfile.dockerignore"
MANAGEMENT_CONFIG = REPOSITORY / "deploy/images/management/container.toml"
MANAGEMENT_ENTRYPOINT = REPOSITORY / "deploy/images/management/entrypoint.sh"
COMPOSE = REPOSITORY / "deploy/compose.yaml"
INSTALLER = REPOSITORY / "scripts/release/install.sh"
WINDOWS_INSTALLER = REPOSITORY / "scripts/release/install.ps1"

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def shell_step(workflow: str, name: str) -> str:
    """Read one literal Bash step in the canonical workflow's bounded layout."""
    marker = "      - name: " + name + "\n"
    if workflow.count(marker) != 1:
        raise ValueError(f"expected exactly one release step {name!r}")
    block = workflow.split(marker, 1)[1].split("      - name:", 1)[0]
    if "        shell: bash\n" not in block or block.count("        run: |\n") != 1:
        raise ValueError(f"release step {name!r} must be one literal Bash block")
    return textwrap.dedent(block.split("        run: |\n", 1)[1])


def job_block(workflow: str, name: str) -> str:
    """Read one top-level job from the canonical workflow's bounded layout."""
    marker = "  " + name + ":\n"
    if workflow.count(marker) != 1:
        raise ValueError(f"expected exactly one release job {name!r}")
    return re.split(r"\n  (?=\S)", workflow.split(marker, 1)[1], maxsplit=1)[0]


def validate(
    workflow: str,
    dockerfile: str,
    dockerignore: str,
    container_config: str,
    entrypoint: str,
    compose: str,
    installer: str,
    windows_installer: str,
    release_workflows: tuple[str, ...],
) -> list[str]:
    failures: list[str] = []

    # Cause/effect graph: one protected semver tag + passing quality gate ->
    # four canonical archives + Sandbox image + Management image -> attestations
    # -> one GitHub Release. Any missing edge, mutable action, competing workflow,
    # or container launcher drift blocks publication.
    # Decision table: R1 complete single graph -> accept; R2 missing artifact/job
    # edge -> reject; R3 competing release workflow -> reject; R4 Management
    # image/Compose drift -> reject; R5 POSIX installer verification drift ->
    # reject; R6 Windows installer verification drift -> reject; R7 only the
    # canonical local quality workflow may be reused without an action SHA.
    required_workflow = (
        "name: release-awaken",
        '- "v*.*.*"',
        "group: release-awaken-${{ github.ref }}",
        "quality:",
        "uses: ./.github/workflows/quality.yml",
        "binary_packages:",
        "sandbox_image:",
        "management_image:",
        "github_release:",
        "needs: quality",
        "needs: [binary_packages, sandbox_image, management_image]",
        "python3 scripts/release/package.py --target",
        "scripts/e2e/management_image_e2e.sh",
        "MANAGEMENT_IMAGE_REPOSITORY: ghcr.io/awakenworks/awaken",
        "IMAGE_REPOSITORY: ghcr.io/awakenworks/awaken-sandbox",
        "actions/attest@59d89421af93a897026c735860bf21b6eb4f7b26",
        "cp scripts/release/install.sh dist/install.sh",
        "sha256sum install.sh > install.sh.sha256",
        "cp scripts/release/install.ps1 dist/install.ps1",
        "sha256sum install.ps1 > install.ps1.sha256",
        'gh release create "$GITHUB_REF_NAME"',
    )
    for marker in required_workflow + TARGETS:
        if marker not in workflow:
            failures.append(f"release workflow missing {marker!r}")
    for line in workflow.splitlines():
        stripped = line.strip()
        if stripped.startswith("uses:"):
            action = stripped.split("#", 1)[0].removeprefix("uses:").strip()
            if action == "./.github/workflows/quality.yml":
                continue
            reference = action.rsplit("@", 1)[-1].strip()
            if len(reference) != 40 or any(char not in "0123456789abcdef" for char in reference):
                failures.append(f"release workflow action is not pinned: {stripped}")
    if release_workflows != (".github/workflows/release.yml",):
        failures.append(
            "expected one canonical release workflow, got " + ", ".join(release_workflows)
        )

    # R8: both consumers must have the pinned Rust toolchain needed by the one
    # proof-checked admission kernel, then execute the canonical validator with
    # exact requested profile and completed proof execution. The validator makes
    # a requested Verified profile imply fresh exact TCB admission; removing the
    # profile binding would therefore bypass that stronger gate. Comments, a
    # command after extract, or a missing strict flag cannot satisfy this audit.
    # Actual shell decision-table tests live beside the production validator.
    pinned_toolchain = (
        "      - uses: dtolnay/rust-toolchain@"
        "6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772\n"
        "        with:\n"
        "          toolchain: 1.96.0"
    )
    for job in ("management_image", "github_release"):
        try:
            if job_block(workflow, job).count(pinned_toolchain) != 1:
                raise ValueError(
                    f"{job}: missing exact release-admission Rust toolchain"
                )
        except ValueError as error:
            failures.append(str(error))
    selection = ["python3", "../scripts/release/verification_manifest.py", "validate",
                 "--artifact", "$archive", "--manifest", "$archive.verification.json",
                 "--expected-profile", "$AWAKEN_RELEASE_PROFILE", "--require-proof-execution"]
    for name in ("Stage the exact packaged binary", "Validate every packaged proof manifest"):
        try:
            script = shell_step(workflow, name)
            commands = [shlex.split(line, comments=True) for line in script.replace("\\\n", " ").splitlines()]
            if commands.count(selection) != 1 or ["set", "-euo", "pipefail"] not in commands:
                raise ValueError(f"{name}: missing exact fail-closed manifest validation")
            if name.startswith("Stage") and commands.index(selection) >= commands.index(["tar", "-xzf", "$archive"]):
                raise ValueError("Management package extraction precedes proof validation")
        except ValueError as error:
            failures.append(str(error))
    if "      - name: Validate every packaged proof manifest\n" in workflow and "      - name: Attest the release archives\n" in workflow:
        if workflow.index("      - name: Validate every packaged proof manifest\n") >= workflow.index("      - name: Attest the release archives\n"):
            failures.append("release attestation precedes proof validation")

    required_installer = (
        "usage: install.sh vMAJOR.MINOR.PATCH",
        "x86_64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "--proto '=https' --tlsv1.2",
        'sha256sum -c "$archive.sha256"',
        'shasum -a 256 -c "$archive.sha256"',
        'tar -xOzf "$temporary/$archive" "$package_root/awaken"',
        'tar -xOzf "$temporary/$archive" "$package_root/awaken-sandbox"',
        'reported=$("$candidate" --version)',
        '"$candidate_companion" hand --check',
        'mv -f -- "$staged_companion" "$install_dir/awaken-sandbox"',
        'mv -f -- "$staged" "$install_dir/awaken"',
    )
    for marker in required_installer:
        if marker not in installer:
            failures.append(f"POSIX installer missing {marker!r}")

    required_windows_installer = (
        "x86_64-pc-windows-msvc",
        "Invoke-WebRequest -UseBasicParsing",
        "Get-FileHash -Algorithm SHA256",
        "awaken.exe",
        "awaken-sandbox.exe",
        "& $candidate --version",
        "& $candidateCompanion hand --check",
        "Move-Item -Force -LiteralPath $stagedCompanion",
        "Move-Item -Force -LiteralPath $stagedAwaken",
    )
    for marker in required_windows_installer:
        if marker not in windows_installer:
            failures.append(f"Windows installer missing {marker!r}")

    required_dockerfile = (
        "FROM ubuntu:24.04@sha256:",
        "ARG BIN=.awaken-management.bin",
        "COPY ${BIN} /usr/local/bin/awaken",
        "COPY .awaken-verification.json /usr/share/awaken/verification.json",
        "chmod 0444 /usr/share/awaken/verification.json",
        "COPY container.toml /etc/awaken/config.toml",
        "USER awaken",
        "COPY entrypoint.sh /usr/local/bin/awaken-container-entrypoint",
        'ENTRYPOINT ["/usr/local/bin/awaken-container-entrypoint"]',
        'CMD ["all-in-one", "--config", "/etc/awaken/config.toml"]',
    )
    for marker in required_dockerfile:
        if marker not in dockerfile:
            failures.append(f"Management Dockerfile missing {marker!r}")
    if dockerignore.splitlines() != [
        "*",
        "!.awaken-management.bin",
        "!.awaken-verification.json",
        "!container.toml",
        "!entrypoint.sh",
    ]:
        failures.append("Management Dockerfile must receive only its binary, proof manifest and typed preset")
    for marker in (
        'SOURCE_REVISION="$(git rev-parse "$GITHUB_SHA^{commit}")"',
        'cosign verify \\',
        'cmp deploy/images/management/.awaken-management.bin',
        'cmp deploy/images/management/.awaken-verification.json',
        'cp "$archive.verification.json" ../deploy/images/management/.awaken-verification.json',
        'docker cp "$container:/usr/share/awaken/verification.json"',
        'docker pull "$image"',
        "deploy/images/management\n",
    ):
        if marker not in workflow:
            failures.append(f"Management image reuse path missing {marker!r}")
    try:
        script = shell_step(workflow, "Build, verify, and publish the Management image")
        commands = [shlex.split(line, comments=True) for line in script.replace("\\\n", " ").splitlines()]
        for selection in (["verify_management_payload()", "{"], ["verify_management_payload", "$image"],
                          ["docker", "pull", "$image"],
                          ["AWAKEN_MANAGEMENT_IMAGE=$image", "scripts/e2e/management_image_e2e.sh"]):
            if commands.count(selection) != 1:
                failures.append(f"Management payload has no unique exact selection: {selection}")
        if ["verify_management_payload", "$STAGING_TAG"] in commands:
            failures.append("Management payload checks a mutable staging tag")
        if '--certificate-github-workflow-sha "$SOURCE_REVISION"' not in script:
            failures.append("Management signature is not pinned to the source workflow commit")
    except ValueError as error:
        failures.append(str(error))
    if "role = \"all-in-one\"" not in container_config or "bind = \"0.0.0.0:8080\"" not in container_config:
        failures.append("Management container config must own the all-in-one container preset")
    for marker in (
        'sandbox_tier = "local"',
        'worker_request_credential_file = "/var/lib/awaken/worker-transport.json"',
        'worker_trust_credentials_file = "/var/lib/awaken/worker-transport.json"',
    ):
        if marker not in container_config:
            failures.append(f"Management container config missing {marker!r}")
    for marker in (
        'if test ! -e "$credential"',
        "openssl rand -base64 32",
        'umask 077',
        'exec /usr/local/bin/awaken "$@"',
    ):
        if marker not in entrypoint:
            failures.append(f"Management entrypoint missing {marker!r}")
    for marker in (
        "ghcr.io/awakenworks/awaken:${AWAKEN_VERSION:-v1.0.0}",
        "awaken-data:/var/lib/awaken",
    ):
        if marker not in compose:
            failures.append(f"Compose quickstart missing {marker!r}")
    return failures


def current_inputs() -> tuple[str, str, str, str, str, str, str, str, tuple[str, ...]]:
    release_workflows = tuple(
        path.relative_to(REPOSITORY).as_posix()
        for path in sorted((REPOSITORY / ".github/workflows").glob("*release*.yml"))
    )
    return (
        WORKFLOW.read_text(encoding="utf-8"),
        MANAGEMENT_DOCKERFILE.read_text(encoding="utf-8"),
        MANAGEMENT_DOCKERIGNORE.read_text(encoding="utf-8"),
        MANAGEMENT_CONFIG.read_text(encoding="utf-8"),
        MANAGEMENT_ENTRYPOINT.read_text(encoding="utf-8"),
        COMPOSE.read_text(encoding="utf-8"),
        INSTALLER.read_text(encoding="utf-8"),
        WINDOWS_INSTALLER.read_text(encoding="utf-8"),
        release_workflows,
    )


@contextmanager
def management_fixture(fault: str, *, reuse: bool = False):
    """One synthetic Docker adapter for payload and full workflow regressions."""
    shim_source = python_script_header(sys.executable) + r'''
import json, os, pathlib, shutil, sys
sys.stdout.reconfigure(newline="\n")
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
tool = pathlib.Path(sys.argv[0]).name
fault = os.environ["FAULT"]
digest = "sha256:" + "a" * 64
with (root / "calls").open("a") as output:
    output.write(json.dumps([tool, *args]) + "\n")
if tool == "git":
    print("revision")
elif tool == "cosign":
    if fault == "signature":
        raise SystemExit("signature failure")
elif tool == "management_image_e2e.sh":
    with (root / "calls").open("a") as output:
        output.write(json.dumps(["e2e", os.environ["AWAKEN_MANAGEMENT_IMAGE"]]) + "\n")
    if fault == "e2e":
        raise SystemExit("E2E failure")
elif args[:2] == ["image", "inspect"]:
    field = next(key for key in ("source", "revision", "version") if "image." + key in args[3])
    print("wrong" if fault == field else {"source": "source", "revision": "revision", "version": "v1"}[field])
elif args[0] == "create":
    (root / "created-image").write_text(args[1])
    print("fixture-container")
elif args[0] == "cp":
    name = "binary" if args[1].endswith("/awaken") else "manifest"
    if fault == "published-manifest" and name == "manifest" and "@" in (root / "created-image").read_text():
        pathlib.Path(args[2]).write_bytes(b"substituted registry payload")
    else:
        shutil.copyfile(root / ("image-" + name), args[2])
elif args[0] == "rm":
    assert "--volumes" in args, "anonymous image volumes must be reclaimed"
    with (root / "removed").open("a") as output: output.write("removed\n")
elif args[:3] == ["buildx", "imagetools", "inspect"]:
    marker = root / "inspected"
    first = not marker.exists()
    marker.touch()
    if first and os.environ["REUSE"] == "0":
        raise SystemExit("manifest unknown")
    print("Digest: " + ("not-a-digest" if fault == "digest" else digest))
elif args[:3] == ["buildx", "imagetools", "create"]:
    pass
elif args[0] == "build":
    pass
elif args[0] == "push":
    print("fixture: digest: " + ("not-a-digest" if fault == "digest" else digest) + " size: 1")
elif args[0] == "pull":
    if fault == "pull":
        raise SystemExit("pull failed")
else:
    raise SystemExit("unexpected adapter call: " + repr(args))
'''
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        context = root / "deploy/images/management"
        context.mkdir(parents=True)
        (context / ".awaken-management.bin").write_bytes(b"binary")
        (context / ".awaken-verification.json").write_bytes(b"manifest")
        (root / "image-binary").write_bytes(b"wrong" if fault == "binary" else b"binary")
        if fault != "missing-manifest":
            (root / "image-manifest").write_bytes(b"wrong" if fault == "manifest" else b"manifest")
        (root / "verification-from-management-image.json").write_bytes(b"manifest")
        shim = root / "adapter"
        shim.write_text(shim_source, encoding="utf-8", newline="\n")
        shim.chmod(0o755)
        for tool in ("docker", "cosign", "git"):
            alias = root / tool
            shutil.copyfile(shim, alias)
            alias.chmod(0o755)
        e2e = root / "scripts/e2e/management_image_e2e.sh"
        e2e.parent.mkdir(parents=True)
        shutil.copyfile(shim, e2e)
        e2e.chmod(0o755)
        setup = root / "fixture-environment.sh"
        setup.write_text('fixture_bin=$(cd -- "$FIXTURE_ROOT" && pwd -P)\n'
                         'export PATH="$fixture_bin:$PATH"\n', encoding="utf-8", newline="\n")
        yield root, dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                         FIXTURE_ROOT=root.as_posix(), BASH_ENV=setup.as_posix(),
                         FAULT=fault, REUSE=str(int(reuse)), RUNNER_TEMP=root.as_posix(),
                         SOURCE_REPOSITORY="source", SOURCE_REVISION="revision", GITHUB_REF_NAME="v1",
                         GITHUB_SHA="revision", GITHUB_REPOSITORY="awakenworks/awaken",
                         GITHUB_REF="refs/tags/v1", GITHUB_RUN_ID="1", GITHUB_RUN_ATTEMPT="1",
                         GITHUB_OUTPUT=(root / "outputs").as_posix(), WORKFLOW_PATH=".github/workflows/release.yml",
                         SIGSTORE_OIDC_ISSUER="https://token.actions.githubusercontent.com",
                         MANAGEMENT_IMAGE_REPOSITORY="ghcr.io/awakenworks/awaken")


class ProductReleaseCheckerTests(unittest.TestCase):
    def test_actual_management_workflow_checks_the_promoted_digest(self) -> None:
        # Causes: new/reused image; payload at the eventual digest equal/different;
        # pull, E2E, signature success/failure; valid/malformed digest.
        # R1 all valid -> check and run exactly the promoted digest, then publish.
        # R2 changed payload -> no E2E, signing or promotion. Old code checked a
        # mutable staging tag, so a different push result escaped its comparison.
        # R3 any listed adapter failure -> no promotion/output. Signature verification
        # for reused images precedes execution; signing new images follows E2E.
        # These execute the real workflow Bash with synthetic adapters; no live
        # registry, Docker, E2E runtime or signature verification is claimed.
        script = shell_step(WORKFLOW.read_text(), "Build, verify, and publish the Management image")
        image = "ghcr.io/awakenworks/awaken@sha256:" + "a" * 64
        for reuse in (False, True):
            for fault in ("none", "published-manifest", "pull", "e2e", "signature", "digest"):
                with self.subTest(reuse=reuse, fault=fault), management_fixture(fault, reuse=reuse) as (root, environment):
                    result = subprocess.run([bash_executable(), "-c", script], cwd=root, env=environment,
                                            capture_output=True, text=True, timeout=30)
                    calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
                    promoted = [call for call in calls if call[:4] == ["docker", "buildx", "imagetools", "create"]]
                    signed = [call for call in calls if call[:2] == ["cosign", "sign"]]
                    if fault != "none":
                        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertFalse(promoted, calls)
                        self.assertFalse((root / "outputs").exists(), calls)
                        if fault in {"published-manifest", "pull", "digest"}:
                            self.assertFalse(signed, calls)
                            self.assertFalse(any(call[0] == "e2e" for call in calls), calls)
                        if reuse and fault == "signature":
                            self.assertFalse(any(call[0] == "e2e" for call in calls), calls)
                        continue
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual([call[2] for call in calls if call[:2] == ["docker", "create"]], [image])
                    self.assertEqual([call[1] for call in calls if call[0] == "e2e"], [image])
                    self.assertEqual(len(promoted), int(not reuse))
                    self.assertEqual(len(signed), int(not reuse))
                    if signed:
                        self.assertEqual(signed[0][-1], image)
                        self.assertEqual(promoted[0][-1], image)
                        self.assertLess(calls.index(["e2e", image]), calls.index(signed[0]))
                    if reuse:
                        verified = next(call for call in calls if call[:2] == ["cosign", "verify"])
                        self.assertEqual(verified, ["cosign", "verify", "--certificate-identity",
                            "https://github.com/awakenworks/awaken/.github/workflows/release.yml@refs/tags/v1",
                            "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
                            "--certificate-github-workflow-sha", "revision", image])
                        self.assertLess(calls.index(verified), calls.index(["docker", "create", image]))

    def test_image_proof_carrier_and_both_consumers_are_mandatory(self) -> None:
        # M1 removing carrier COPY/readonly mode/build-context allowlist or
        # the common immutable validator call rejects. A comment mentioning
        # the omitted call is not executable linkage to the same authority.
        inputs = current_inputs()
        self.assertEqual(validate(*inputs), [])
        mutations = [
            (inputs[0], inputs[1].replace("COPY .awaken-verification.json", "# omitted carrier"), *inputs[2:]),
            (inputs[0], inputs[1].replace("chmod 0444", "chmod 0666"), *inputs[2:]),
            (*inputs[:2], inputs[2].replace("!.awaken-verification.json\n", ""), *inputs[3:]),
        ]
        for command in ('verify_management_payload "$image"', 'docker pull "$image"',
                        'AWAKEN_MANAGEMENT_IMAGE="$image" scripts/e2e/management_image_e2e.sh'):
            mutations.append((inputs[0].replace(command, "# " + command), *inputs[1:]))
        for index, mutation in enumerate(mutations):
            with self.subTest(rule=index):
                self.assertTrue(validate(*mutation))

    def test_actual_management_payload_requires_exact_binary_manifest_and_labels(self) -> None:
        # Cause/effect: all three labels + exact binary + exact original
        # manifest => reuse/build may continue. Any missing/mismatched input
        # rejects. Creation starts one temporary container; failure after create
        # must still remove it. A stale copied output cannot mask a failed cp.
        # Execute the actual shared shell function with a local Docker adapter;
        # this is not a real registry, signature or container-runtime proof.
        script = shell_step(WORKFLOW.read_text(), "Build, verify, and publish the Management image")
        body = script.split("verify_management_payload() {\n", 1)[1].split("\n}", 1)[0]
        function = "verify_management_payload() {\n" + body + "\n}\n"
        for fault in ("none", "source", "revision", "version", "binary", "manifest", "missing-manifest"):
            with self.subTest(fault=fault), management_fixture(fault) as (root, environment):
                result = subprocess.run([bash_executable(), "-c", 'set -euo pipefail\n' + function + 'verify_management_payload "fixture-image"\n'],
                                        cwd=root, capture_output=True, text=True, timeout=30, env=environment)
                if fault == "none":
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                else:
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                if fault in {"none", "binary", "manifest", "missing-manifest"}:
                    self.assertTrue((root / "removed").exists(), result.stderr)
                    self.assertEqual((root / "removed").read_text(), "removed\n", result.stderr)
                else:
                    self.assertFalse((root / "removed").exists())

    def test_proof_consumer_binding_decision_table(self) -> None:
        # Cause/effect table: C1 each consumer has the pinned kernel toolchain;
        # C2 names the requested profile; C3 requires completed proof execution;
        # C4 runs the canonical validator; C5 Management validates before
        # extraction. R1 all true accepts. R2 removes C1-C4 independently and
        # rejects. R3 moves C5 after extraction and rejects. This preserves the
        # edge from a Verified request through the production Rust disposition
        # to mandatory TCB evidence admission.
        inputs = current_inputs()
        self.assertEqual(validate(*inputs), [])
        pinned = (
            "      - uses: dtolnay/rust-toolchain@"
            "6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772\n"
            "        with:\n"
            "          toolchain: 1.96.0\n"
        )
        for job in ("management_image", "github_release"):
            block = job_block(inputs[0], job)
            with self.subTest(job=job, cause="toolchain"):
                self.assertTrue(validate(
                    inputs[0].replace(block, block.replace(pinned, "", 1), 1),
                    *inputs[1:],
                ))
        for name in ("Stage the exact packaged binary", "Validate every packaged proof manifest"):
            marker = "      - name: " + name + "\n"
            start, rest = inputs[0].split(marker, 1)
            block, end = rest.split("      - name:", 1)
            for changed in (block.replace('--expected-profile "$AWAKEN_RELEASE_PROFILE"', ""),
                            block.replace("--require-proof-execution", ""),
                            block.replace("python3 ../scripts/release/verification_manifest.py validate", "# omitted validator")):
                with self.subTest(step=name):
                    self.assertTrue(validate(start + marker + changed + "      - name:" + end, *inputs[1:]))
        workflow = inputs[0].replace('          tar -xzf "$archive"\n', "")
        workflow = workflow.replace('          archive="${checksums[0]%.sha256}"\n',
                                    '          archive="${checksums[0]%.sha256}"\n          tar -xzf "$archive"\n')
        self.assertIn("Management package extraction precedes proof validation", validate(workflow, *inputs[1:]))

    def test_decision_table_rejects_each_release_authority_drift(self) -> None:
        # The validation comment above owns the cause/effect graph. These
        # mutations execute R1-R6: baseline, broken job edge, competing workflow,
        # container launcher drift, and each platform installer's verification drift.
        inputs = current_inputs()
        self.assertEqual(validate(*inputs), [], "R1")
        mutations = (
            (inputs[0].replace("github_release:", "release_assets:"), *inputs[1:]),
            (*inputs[:8], (".github/workflows/release.yml", ".github/workflows/release-copy.yml")),
            (inputs[0], inputs[1].replace("USER awaken", "USER root"), *inputs[2:]),
            (*inputs[:6], inputs[6].replace("sha256sum -c", "true #"), *inputs[7:]),
            (
                *inputs[:7],
                inputs[7].replace(
                    "Get-FileHash -Algorithm SHA256", "Write-Host skipped"
                ),
                inputs[8],
            ),
            (
                inputs[0].replace(
                    "uses: ./.github/workflows/quality.yml",
                    "uses: ./.github/workflows/quality-copy.yml",
                ),
                *inputs[1:],
            ),
        )
        for rule, mutation in enumerate(mutations, 2):
            with self.subTest(rule=f"R{rule}"):
                self.assertTrue(validate(*mutation))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ProductReleaseCheckerTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    failures = validate(*current_inputs())
    if failures:
        print("\n".join(f"ERROR: {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("product release contract: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
