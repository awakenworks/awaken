#!/usr/bin/env python3
"""Keep one Open-owned, digest-bound, keyless Sandbox image release path."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW = REPO_ROOT / ".github/workflows/release.yml"
RELEASE_SCRIPTS = REPO_ROOT / "scripts/release"

ACTION_PINS = (
    "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803",
    "docker/login-action@dbcb813823bdd20940b903addbd779551569679f",
    "docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e",
    "sigstore/cosign-installer@ba7bc0a3fef59531c69a25acd34668d6d3fe6f22",
)
PREDICATE_TYPE = (
    "https://awakenworks.com/attestations/awaken-sandbox-image-provenance/v1"
)
IMAGE_REPOSITORY = "ghcr.io/awakenworks/awaken-sandbox"
CANONICAL_BUILD = 'deploy/images/sandbox/build.sh "$RELEASE_TAG"'
PINNED_ACTION_RE = re.compile(r"uses:\s+\S+@[0-9a-f]{40}(?:\s+#.*)?")


def validate(
    workflow: str,
    workflow_sources: dict[str, str],
    release_sources: dict[str, str],
) -> list[str]:
    failures: list[str] = []

    # Cause/effect graph:
    # protected exact tag/revision + canonical build owner -> one accepted image
    # one push -> immutable digest -> exact OIDC signature and Open predicate
    # alternate build/publisher or mutable tag/action/key -> competing authority
    # wrong identity/issuer/predicate or use of the tag after resolution -> unbound proof
    #
    # Decision table (self_test owns every negative rule):
    # sole owner | protected exact source | pinned/keyless | digest-only | effect
    #     1      |          1             |       1        |      1      | accept
    #     0/2+   |          *             |       *        |      *      | reject
    #     1      |          0             |       *        |      *      | reject
    #     1      |          1             |       0        |      *      | reject
    #     1      |          1             |       1        |      0      | reject
    required = (
        "name: release-awaken-sandbox-image",
        "tags:",
        '- "v*.*.*"',
        "if: github.repository == 'awakenworks/awaken' && github.ref_protected == true",
        "contents: read",
        "packages: write",
        "id-token: write",
        "persist-credentials: false",
        f"IMAGE_REPOSITORY: {IMAGE_REPOSITORY}",
        f"PREDICATE_TYPE: {PREDICATE_TYPE}",
        "SIGSTORE_OIDC_ISSUER: https://token.actions.githubusercontent.com",
        "WORKFLOW_PATH: .github/workflows/release.yml",
        "CONTAINER_ENGINE: docker",
        'test "$GITHUB_REF_TYPE" = "tag"',
        'test "$GITHUB_REF_PROTECTED" = "true"',
        "cosign-release: v3.0.6",
        "python3 scripts/release/awaken_sandbox_image_provenance.py self-test",
        "python3 scripts/ci/check_sandbox_image_release.py --self-test",
        "python3 scripts/ci/check_sandbox_image_release.py",
        "python3 scripts/release/awaken_sandbox_image_provenance.py validate-context",
        'SOURCE_REVISION="$(git rev-parse "$GITHUB_SHA^{commit}")"',
        'test "$(git rev-parse HEAD)" = "$SOURCE_REVISION"',
        '--revision "$SOURCE_REVISION"',
        '--source-ref "$GITHUB_REF"',
        '--workflow-ref "$GITHUB_WORKFLOW_REF"',
        CANONICAL_BUILD,
        'docker push "$RELEASE_TAG"',
        "docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}'",
        'cosign sign --yes "$IMAGE"',
        "cosign attest --yes",
        "cosign verify-attestation",
        '--certificate-identity "$CERTIFICATE_IDENTITY"',
        '--certificate-oidc-issuer "$SIGSTORE_OIDC_ISSUER"',
        "validate-attestations",
        'cmp "$RUNNER_TEMP/awaken-sandbox-image-provenance.json"',
    ) + ACTION_PINS
    for marker in required:
        if marker not in workflow:
            failures.append(f".github/workflows/release.yml: missing {marker!r}")

    exact_counts = {
        CANONICAL_BUILD: 1,
        'docker push "$RELEASE_TAG"': 1,
        "cosign sign --yes": 1,
        "cosign attest --yes": 1,
        "cosign verify-attestation": 1,
        "awaken_sandbox_image_provenance.py emit": 1,
        "validate-attestations": 1,
    }
    for marker, expected in exact_counts.items():
        actual = workflow.count(marker)
        if actual != expected:
            failures.append(
                ".github/workflows/release.yml: expected "
                f"{expected} occurrence(s) of {marker!r}, got {actual}"
            )

    forbidden = (
        "workflow_dispatch:",
        "workflow_call:",
        "workflow_run:",
        "repository_dispatch:",
        "pull_request:",
        "schedule:",
        "\n  release:",
        "branches:",
        f"{IMAGE_REPOSITORY}:latest",
        "docker/build-push-action@",
        "docker build ",
        "docker buildx build",
        "docker tag ",
        "oras push ",
        "crane push ",
        "skopeo copy ",
        "provenance: true",
        "provenance: mode=",
        "GIT_AUTH_TOKEN",
        "github_token=",
        "COSIGN_PRIVATE_KEY",
        "cosign.key",
        "--key ",
        "--certificate-identity-regexp",
        "--insecure",
        "--check-claims=false",
        "--no-check-claims",
        'cosign sign --yes "$RELEASE_TAG"',
        'cosign attest --yes "$RELEASE_TAG"',
    )
    for marker in forbidden:
        if marker in workflow:
            failures.append(f".github/workflows/release.yml: forbidden {marker!r}")

    lines = workflow.splitlines()
    try:
        permissions_index = lines.index("    permissions:")
    except ValueError:
        permissions = []
    else:
        permissions = []
        for line in lines[permissions_index + 1 :]:
            if not line.startswith("      "):
                break
            if line.strip():
                permissions.append(line.strip())
    expected_permissions = ["contents: read", "packages: write", "id-token: write"]
    if permissions != expected_permissions:
        failures.append(
            ".github/workflows/release.yml: expected exact job permissions "
            f"{expected_permissions}, got {permissions}"
        )
    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if stripped.startswith("uses:") and PINNED_ACTION_RE.fullmatch(stripped) is None:
            failures.append(
                ".github/workflows/release.yml: "
                f"line {line_number} action must use an immutable 40-hex commit pin"
            )

    resolution_marker = (
        "IMAGE=\"$(docker image inspect --format "
        "'{{range .RepoDigests}}{{println .}}{{end}}' \"$RELEASE_TAG\")\""
    )
    resolution_index = workflow.find(resolution_marker)
    if resolution_index >= 0:
        after_resolution = workflow[workflow.find("\n", resolution_index) + 1 :]
        if '"$RELEASE_TAG"' in after_resolution:
            failures.append(
                ".github/workflows/release.yml: release tag reused after immutable "
                "digest resolution"
            )

    publisher_markers = (
        "docker/build-push-action@",
        "docker push ",
        "cosign attest",
        "oras push ",
        "crane push ",
        "skopeo copy ",
    )
    publishers = sorted(
        path
        for path, source in workflow_sources.items()
        if any(marker in source for marker in publisher_markers)
    )
    if publishers != [".github/workflows/release.yml"]:
        failures.append(
            f"Awaken Sandbox must have exactly one workflow publisher, got {publishers}"
        )

    local_publisher_markers = (
        "docker push ",
        "buildx build",
        "cosign sign",
        "cosign attest",
        "oras push ",
        "crane push ",
        "skopeo copy ",
    )
    for path, source in release_sources.items():
        for marker in local_publisher_markers:
            if marker in source:
                failures.append(
                    f"{path}: local image publisher is forbidden ({marker!r})"
                )
    return failures


def _workflow_sources() -> dict[str, str]:
    root = REPO_ROOT / ".github/workflows"
    sources: dict[str, str] = {}
    if not root.is_dir():
        return sources
    for pattern in ("*.yml", "*.yaml"):
        for path in root.glob(pattern):
            sources[str(path.relative_to(REPO_ROOT))] = path.read_text(encoding="utf-8")
    return sources


def _release_sources() -> dict[str, str]:
    if not RELEASE_SCRIPTS.is_dir():
        return {}
    return {
        str(path.relative_to(REPO_ROOT)): path.read_text(encoding="utf-8")
        for path in RELEASE_SCRIPTS.iterdir()
        if path.is_file() and path.suffix in {".py", ".sh"}
    }


def self_test(workflow: str) -> list[str]:
    failures: list[str] = []
    baseline_sources = {".github/workflows/release.yml": workflow}
    release_sources = {
        "scripts/release/awaken_sandbox_image_provenance.py": "validator only"
    }
    if validate(workflow, baseline_sources, release_sources):
        failures.append("baseline release workflow must satisfy its own checker")

    cases = (
        (
            "missing package permission",
            workflow.replace("packages: write", "packages: read"),
        ),
        (
            "extra action permission",
            workflow.replace("id-token: write", "id-token: write\n      actions: write"),
        ),
        (
            "unprotected ref",
            workflow.replace("github.ref_protected == true", "github.ref_protected == false"),
        ),
        (
            "missing canonical build owner",
            workflow.replace(CANONICAL_BUILD, 'docker build -t "$RELEASE_TAG" .'),
        ),
        ("mutable action", workflow.replace(ACTION_PINS[0], "actions/checkout@v6")),
        (
            "wrong image repository",
            workflow.replace(IMAGE_REPOSITORY, "ghcr.io/example/parallel-sandbox"),
        ),
        (
            "wrong predicate type",
            workflow.replace(PREDICATE_TYPE, "https://example.invalid/provenance"),
        ),
        (
            "identity regexp",
            workflow.replace(
                "--certificate-identity ", "--certificate-identity-regexp "
            ),
        ),
        (
            "insecure verification",
            workflow.replace("cosign verify \\\n", "cosign verify --insecure \\\n"),
        ),
        (
            "private signing key",
            workflow.replace("cosign sign --yes", "cosign sign --yes --key cosign.key"),
        ),
        (
            "mutable-tag signing",
            workflow.replace('cosign sign --yes "$IMAGE"', 'cosign sign --yes "$RELEASE_TAG"'),
        ),
        (
            "manual trigger",
            workflow.replace("  push:\n", "  workflow_dispatch:\n  push:\n"),
        ),
        (
            "branch trigger",
            workflow.replace("    tags:\n", "    branches:\n      - main\n    tags:\n"),
        ),
        (
            "latest tag",
            workflow.replace(
                'RELEASE_TAG="${IMAGE_REPOSITORY}:${GITHUB_REF_NAME}"',
                f'RELEASE_TAG="{IMAGE_REPOSITORY}:latest"',
            ),
        ),
    )
    for name, candidate in cases:
        if not validate(
            candidate,
            {".github/workflows/release.yml": candidate},
            release_sources,
        ):
            failures.append(f"{name}: expected checker refusal")

    duplicate_sources = baseline_sources | {
        ".github/workflows/parallel-release.yml": "docker push parallel-image"
    }
    if not validate(workflow, duplicate_sources, release_sources):
        failures.append("parallel workflow publisher: expected checker refusal")

    local_sources = release_sources | {
        "scripts/release/push-image.sh": "docker push image"
    }
    if not validate(workflow, baseline_sources, local_sources):
        failures.append("local publisher: expected checker refusal")
    return failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if not WORKFLOW.is_file():
        print("Awaken Sandbox image release contract violated:", file=sys.stderr)
        print("  .github/workflows/release.yml: missing sole publisher", file=sys.stderr)
        return 1
    workflow = WORKFLOW.read_text(encoding="utf-8")
    if args.self_test:
        failures = self_test(workflow)
        if failures:
            print("Awaken Sandbox release checker self-test failed:", file=sys.stderr)
            for failure in failures:
                print(f"  {failure}", file=sys.stderr)
            return 2
        print("OK - Awaken Sandbox release checker self-test passed.")
        return 0

    failures = validate(workflow, _workflow_sources(), _release_sources())
    if failures:
        print("Awaken Sandbox image release contract violated:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - one Open-owned immutable Sandbox image release path is enforced.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
