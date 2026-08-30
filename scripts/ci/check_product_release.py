#!/usr/bin/env python3
"""Validate the one tag-driven Awaken product release graph."""

from __future__ import annotations

import argparse
import sys
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY / ".github/workflows/release.yml"
MANAGEMENT_DOCKERFILE = REPOSITORY / "deploy/images/management/Dockerfile"
MANAGEMENT_DOCKERIGNORE = REPOSITORY / "deploy/images/management/Dockerfile.dockerignore"
MANAGEMENT_CONFIG = REPOSITORY / "deploy/images/management/container.toml"
MANAGEMENT_ENTRYPOINT = REPOSITORY / "deploy/images/management/entrypoint.sh"
COMPOSE = REPOSITORY / "deploy/compose.yaml"

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def validate(
    workflow: str,
    dockerfile: str,
    dockerignore: str,
    container_config: str,
    entrypoint: str,
    compose: str,
    release_workflows: tuple[str, ...],
) -> list[str]:
    failures: list[str] = []

    # Cause/effect graph: one protected semver tag + passing quality gate ->
    # four canonical archives + Sandbox image + Management image -> attestations
    # -> one GitHub Release. Any missing edge, mutable action, competing workflow,
    # or container launcher drift blocks publication.
    # Decision table: R1 complete single graph -> accept; R2 missing artifact/job
    # edge -> reject; R3 competing release workflow -> reject; R4 Management
    # image/Compose not using the canonical binary/config/repository -> reject.
    required_workflow = (
        "name: release-awaken",
        '- "v*.*.*"',
        "group: release-awaken-${{ github.ref }}",
        "quality:",
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
        'gh release create "$GITHUB_REF_NAME"',
    )
    for marker in required_workflow + TARGETS:
        if marker not in workflow:
            failures.append(f"release workflow missing {marker!r}")
    for line in workflow.splitlines():
        stripped = line.strip()
        if stripped.startswith("uses:"):
            reference = stripped.split("#", 1)[0].rsplit("@", 1)[-1].strip()
            if len(reference) != 40 or any(char not in "0123456789abcdef" for char in reference):
                failures.append(f"release workflow action is not pinned: {stripped}")
    if release_workflows != (".github/workflows/release.yml",):
        failures.append(
            "expected one canonical release workflow, got " + ", ".join(release_workflows)
        )

    required_dockerfile = (
        "FROM ubuntu:24.04@sha256:",
        "ARG BIN=.awaken-management.bin",
        "COPY ${BIN} /usr/local/bin/awaken",
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
        "!container.toml",
        "!entrypoint.sh",
    ]:
        failures.append("Management Dockerfile must receive only its binary and typed preset")
    for marker in (
        'SOURCE_REVISION="$(git rev-parse "$GITHUB_SHA^{commit}")"',
        'cosign verify \\',
        'cmp deploy/images/management/.awaken-management.bin',
        'docker pull "$image"',
        "deploy/images/management\n",
    ):
        if marker not in workflow:
            failures.append(f"Management image reuse path missing {marker!r}")
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


def current_inputs() -> tuple[str, str, str, str, tuple[str, ...]]:
    release_workflows = tuple(
        str(path.relative_to(REPOSITORY))
        for path in sorted((REPOSITORY / ".github/workflows").glob("*release*.yml"))
    )
    return (
        WORKFLOW.read_text(encoding="utf-8"),
        MANAGEMENT_DOCKERFILE.read_text(encoding="utf-8"),
        MANAGEMENT_DOCKERIGNORE.read_text(encoding="utf-8"),
        MANAGEMENT_CONFIG.read_text(encoding="utf-8"),
        MANAGEMENT_ENTRYPOINT.read_text(encoding="utf-8"),
        COMPOSE.read_text(encoding="utf-8"),
        release_workflows,
    )


class ProductReleaseCheckerTests(unittest.TestCase):
    def test_decision_table_rejects_each_release_authority_drift(self) -> None:
        # The validation comment above owns the cause/effect graph. These
        # mutations execute R1-R4: baseline, broken job edge, competing workflow,
        # and container launcher drift respectively.
        inputs = current_inputs()
        self.assertEqual(validate(*inputs), [], "R1")
        mutations = (
            (inputs[0].replace("github_release:", "release_assets:"), *inputs[1:]),
            (*inputs[:6], (".github/workflows/release.yml", ".github/workflows/release-copy.yml")),
            (inputs[0], inputs[1].replace("USER awaken", "USER root"), *inputs[2:]),
        )
        for rule, mutation in enumerate(mutations, 2):
            with self.subTest(rule=f"R{rule}"):
                self.assertTrue(validate(*mutation))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
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
