#!/usr/bin/env python3
"""Keep one Open-owned, digest-bound, keyless Sandbox image release path."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW = REPO_ROOT / ".github/workflows/release.yml"
BUILD_SCRIPT = REPO_ROOT / "deploy/images/sandbox/build.sh"
RESOLVER = "scripts/release/resolve_awaken_sandbox_release_image.sh"

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
SOURCE_REPOSITORY = "https://github.com/awakenworks/awaken"
CANONICAL_BUILD = 'deploy/images/sandbox/build.sh "$STAGING_TAG"'
SIGNATURE_CREATE_BLOCK = """            case "$SIGNATURE_STATE" in
              absent)
                cosign sign --yes "$IMAGE"
                query_release_signature "$IMAGE" staging-after-sign
                test "$SIGNATURE_STATE" = reuse
                ;;
              reuse) ;;
              *) echo "unexpected image-signature state: $SIGNATURE_STATE" >&2; exit 1 ;;
            esac
"""
PINNED_ACTION_RE = re.compile(r"uses:\s+\S+@[0-9a-f]{40}(?:\s+#.*)?")
CANONICAL_WORKFLOW = ".github/workflows/release.yml"
EXPECTED_REGISTRY_WRITE_OWNERS = {CANONICAL_WORKFLOW}
EXPECTED_CANONICAL_WRITE_COUNTS = {
    "registry-write workflow permission": 2,
    "Docker registry write": 4,
    "OCI publisher command": 3,
}
REGISTRY_WRITE_PATTERNS = (
    (
        "registry-write workflow permission",
        re.compile(r"(?m)^\s*packages:\s*write\s*(?:#.*)?$"),
    ),
    (
        "registry publisher action",
        re.compile(
            r"(?mi)^\s*uses:\s+\S*(?:build-push|push-to-registry|publish|"
            r"registry|oras|crane|skopeo|regctl)\S*@"
        ),
    ),
    (
        "Docker registry write",
        re.compile(
            r"(?i)\bdocker\s+(?:push\b|image\s+push\b|manifest\s+push\b|"
            r"buildx\s+imagetools\s+create\b)"
        ),
    ),
    (
        "Buildx push",
        re.compile(
            r"(?im)\bdocker\s+buildx\s+(?:build|bake)\b"
            r"(?:(?:[^\n]*\\\s*\n)*[^\n]*)"
            r"(?:--push\b|--output(?:=|\s+)type=registry\b|"
            r"-o(?:=|\s+)type=registry\b|"
            r"--output(?:=|\s+)[\"']?[^\s\\\n\"']*push=true\b|"
            r"-o(?:=|\s+)[\"']?[^\s\\\n\"']*push=true\b)"
        ),
    ),
    (
        "OCI publisher command",
        re.compile(
            r"(?i)\b(?:oras\s+(?:push|copy|cp|attach|manifest\s+push|"
            r"blob\s+push)|crane\s+"
            r"(?:push|copy|cp|append|mutate|tag|index)|skopeo\s+copy|"
            r"regctl\s+(?:image\s+copy|index\s+create|tag)|"
            r"(?:podman|buildah)\s+push|cosign\s+(?:sign|attest|attach|copy)|"
            r"notation\s+sign)\b"
        ),
    ),
)
OCI_DISTRIBUTION_PATH_RE = re.compile(
    r"(?:https?://[^\s\"']+/v2/|(?<![A-Za-z0-9_/-])/v2/)"
)
OCI_DISTRIBUTION_WRITE_METHOD_RE = re.compile(
    r"(?i)(?:\bmethod\s*[=:]\s*[\"'](?:POST|PATCH|PUT|DELETE)[\"']|"
    r"(?:--request|-X)\s*(?:POST|PATCH|PUT|DELETE)\b)"
)


def validate(
    workflow: str,
    build_script: str,
    repository_sources: dict[str, str],
) -> list[str]:
    failures: list[str] = []
    product_workflow = workflow
    sandbox_start = workflow.find("  sandbox_image:\n")
    sandbox_end = workflow.find("\n  management_image:\n", sandbox_start)
    if sandbox_start < 0 or sandbox_end < 0:
        return [".github/workflows/release.yml: missing bounded sandbox_image job"]
    # Global trigger/environment policy plus the bounded Sandbox job form the
    # Sandbox release contract. Other product jobs are checked by the product
    # release checker and must not be mistaken for duplicate Sandbox writers.
    workflow = workflow[: workflow.find("jobs:\n") + len("jobs:\n")] + workflow[
        sandbox_start:sandbox_end
    ]

    # Cause/effect graph:
    # protected exact tag/revision + canonical build owner -> one accepted image
    # absent semver tag -> stage, resolve, query/create/requery signature and
    # predicate independently, verify, then promote the exact digest
    # present tag + exact current-workflow image signature + one exact predicate
    # + exact OCI labels -> reuse its immutable digest without any build
    # present tag + missing/wrong/conflicting proof or labels -> fail closed
    # proof after promotion, tag re-read, mutable action/key, or another publisher
    # -> untrusted or competing release authority
    #
    # Decision table (self_test owns every negative rule):
    # tag | signature S | predicate P | labels | effect
    #  0  |      0      |      0      | exact  | sign/requery, attest/requery
    #  0  |      1      |      0      | exact  | reuse S, attest/requery
    #  0  |      1      |      1      | exact  | reuse both, promote
    #  0  |      0      |      1      |   *    | reject broken order
    #  1  |      1      |      1      | exact  | zero-write reuse; final read
    #  *  |    2+/bad   | 2+/bad/other |   *   | reject ambiguity/drift
    required = (
        "name: release-awaken",
        "tags:",
        '- "v*.*.*"',
        "if: github.repository == 'awakenworks/awaken' && github.ref_protected == true",
        "contents: read",
        "packages: write",
        "id-token: write",
        "persist-credentials: false",
        "group: release-awaken-${{ github.ref }}",
        "cancel-in-progress: false",
        f"IMAGE_REPOSITORY: {IMAGE_REPOSITORY}",
        f"SOURCE_REPOSITORY: {SOURCE_REPOSITORY}",
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
        'STAGING_TAG="${IMAGE_REPOSITORY}:staging-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"',
        "scripts/release/resolve_awaken_sandbox_release_image.sh",
        'initial="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        'staging="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        'before="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        'after="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        'test "$after" = "present $IMAGE"',
        'final_existing="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        'test "$final_existing" = "present $IMAGE"',
        '--certificate-github-workflow-repository "$GITHUB_REPOSITORY"',
        '--certificate-github-workflow-ref "$GITHUB_REF"',
        '--certificate-github-workflow-sha "$SOURCE_REVISION"',
        "--certificate-github-workflow-trigger push",
        'test -s "$raw_attestations"',
        CANONICAL_BUILD,
        'AWAKEN_SANDBOX_IMAGE_SOURCE="$SOURCE_REPOSITORY"',
        'AWAKEN_SANDBOX_IMAGE_REVISION="$SOURCE_REVISION"',
        'AWAKEN_SANDBOX_IMAGE_VERSION="$GITHUB_REF_NAME"',
        'docker push "$STAGING_TAG"',
        'if [[ -s "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" ]]; then',
        "cosign download attestation",
        "if ! cosign download attestation \\",
        '[[ ! -s "$output" ]] && grep -Fqi \'no attestations\' "$download_error"',
        'cat "$download_error" >&2',
        '--predicate-type "$PREDICATE_TYPE"',
        'cosign download signature "$image"',
        "publisher-signature-state",
        'query_release_signature "$IMAGE" existing',
        'query_release_signature "$IMAGE" staging-initial',
        'query_release_signature "$IMAGE" staging-after-sign',
        'test "$SIGNATURE_STATE" = reuse',
        'if [[ -s "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" ]]; then\n'
        '            test "$SIGNATURE_STATE" = reuse',
        SIGNATURE_CREATE_BLOCK,
        'cosign sign --yes "$IMAGE"',
        "cosign attest --yes",
        "cosign verify \\",
        "cosign verify-attestation",
        '--certificate-identity "$CERTIFICATE_IDENTITY"',
        '--certificate-oidc-issuer "$SIGSTORE_OIDC_ISSUER"',
        "validate-attestations",
        'cmp "$RUNNER_TEMP/emitted-awaken-sandbox-image-provenance.json"',
        "docker buildx imagetools create",
        "--prefer-index=false",
        '--tag "$RELEASE_TAG"',
        '--metadata-file "$RUNNER_TEMP/awaken-sandbox-promotion-metadata.json"',
        "docker buildx imagetools create \\\n"
        "                --prefer-index=false \\\n"
        '                --tag "$RELEASE_TAG" \\\n'
        '                --metadata-file "$RUNNER_TEMP/awaken-sandbox-promotion-metadata.json" \\\n'
        '                "$IMAGE"\n'
        "              python3 scripts/release/awaken_sandbox_image_provenance.py \\",
        "validate-promotion",
    ) + ACTION_PINS
    for marker in required:
        if marker not in workflow:
            failures.append(f".github/workflows/release.yml: missing {marker!r}")

    exact_counts = {
        CANONICAL_BUILD: 1,
        'docker push "$STAGING_TAG"': 1,
        "docker push ": 1,
        "cosign sign --yes": 1,
        "cosign attest --yes": 1,
        "cosign verify \\": 1,
        "cosign verify-attestation": 1,
        "cosign download signature": 1,
        "cosign download attestation": 1,
        "publisher-signature-state": 1,
        'query_release_signature "$IMAGE"': 3,
        'test "$SIGNATURE_STATE" = reuse': 3,
        "awaken_sandbox_image_provenance.py emit": 1,
        "validate-attestations": 2,
        'verify_release_proof "$IMAGE"': 3,
        "docker buildx imagetools create": 1,
        "validate-promotion": 1,
        "scripts/release/resolve_awaken_sandbox_release_image.sh": 5,
        "\n                --tag ": 1,
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
        "cancel-in-progress: true",
        "continue-on-error:",
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
        'deploy/images/sandbox/build.sh "$RELEASE_TAG"',
        'docker push "$RELEASE_TAG"',
        'cosign sign --yes "$RELEASE_TAG"',
        'cosign attest --yes "$RELEASE_TAG"',
        'docker manifest inspect "$RELEASE_TAG"',
        "BUILT_IMAGE_ID",
        "EXISTING_IMAGE_ID",
        "if: steps.provenance.outputs.mode == 'attest'",
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

    release_lookup_index = workflow.find(
        'initial="$(scripts/release/resolve_awaken_sandbox_release_image.sh'
    )
    existing_signature_index = workflow.find(
        'query_release_signature "$IMAGE" existing'
    )
    existing_signature_admission_index = workflow.find(
        'test "$SIGNATURE_STATE" = reuse'
    )
    existing_proof_index = workflow.find('verify_release_proof "$IMAGE"')
    final_existing_index = workflow.find(
        'final_existing="$(scripts/release/resolve_awaken_sandbox_release_image.sh'
    )
    existing_exit_index = workflow.find("            exit 0")
    build_index = workflow.find(CANONICAL_BUILD)
    staging_push_index = workflow.find('docker push "$STAGING_TAG"')
    staging_resolution_index = workflow.find(
        'staging="$(scripts/release/resolve_awaken_sandbox_release_image.sh'
    )
    initial_signature_index = workflow.find(
        'query_release_signature "$IMAGE" staging-initial'
    )
    sign_index = workflow.find('cosign sign --yes "$IMAGE"')
    post_sign_query_index = workflow.find(
        'query_release_signature "$IMAGE" staging-after-sign'
    )
    attest_index = workflow.find("cosign attest --yes")
    final_proof_index = workflow.rfind('verify_release_proof "$IMAGE"')
    before_promotion_index = workflow.find(
        'before="$(scripts/release/resolve_awaken_sandbox_release_image.sh'
    )
    promotion_index = workflow.find("docker buildx imagetools create")
    promotion_validation_index = workflow.find("validate-promotion")
    after_promotion_index = workflow.find(
        'after="$(scripts/release/resolve_awaken_sandbox_release_image.sh'
    )
    ordered_indexes = (
        release_lookup_index,
        existing_signature_index,
        existing_signature_admission_index,
        existing_proof_index,
        final_existing_index,
        existing_exit_index,
        build_index,
        staging_push_index,
        staging_resolution_index,
        initial_signature_index,
        sign_index,
        post_sign_query_index,
        attest_index,
        final_proof_index,
        before_promotion_index,
        promotion_index,
        promotion_validation_index,
        after_promotion_index,
    )
    if any(index < 0 for index in ordered_indexes) or list(ordered_indexes) != sorted(
        ordered_indexes
    ):
        failures.append(
            ".github/workflows/release.yml: resolve/reuse before build and "
            "stage/prove before promotion ordering drifted"
        )

    proof_indexes = (
        workflow.find("            cosign verify \\"),
        workflow.find("            cosign verify-attestation \\"),
        workflow.find(
            '--attestations "$RUNNER_TEMP/${prefix}-verified-attestations.json"'
        ),
        workflow.find('--attestations "$raw_attestations"'),
    )
    if any(index < 0 for index in proof_indexes) or list(proof_indexes) != sorted(
        proof_indexes
    ):
        failures.append(
            ".github/workflows/release.yml: exact cryptographic verification and "
            "exactly-one verified validation must precede raw-set comparison"
        )

    build_required = (
        "configure_release_image_labels",
        "if [[ -z $source && -z $revision && -z $version ]]; then",
        "if [[ -z $source || -z $revision || -z $version ]]; then",
        '--label "org.opencontainers.image.source=$source"',
        '--label "org.opencontainers.image.revision=$revision"',
        '--label "org.opencontainers.image.version=$version"',
        "sandbox release image labels must be supplied as one complete set",
    )
    for marker in build_required:
        if marker not in build_script:
            failures.append(f"deploy/images/sandbox/build.sh: missing {marker!r}")
    if build_script.count('"${release_image_labels[@]}"') != 2:
        failures.append(
            "deploy/images/sandbox/build.sh: both canonical build branches must "
            "project the one release-label array"
        )

    registry_write_owners: dict[str, tuple[str, ...]] = {}
    for path, source in repository_sources.items():
        capabilities = _registry_write_capabilities(source)
        if capabilities:
            registry_write_owners[path] = capabilities
    if set(registry_write_owners) != EXPECTED_REGISTRY_WRITE_OWNERS:
        failures.append(
            "Awaken Sandbox registry-write owner inventory drifted: expected "
            f"{sorted(EXPECTED_REGISTRY_WRITE_OWNERS)}, got "
            f"{sorted(registry_write_owners)}"
        )
    canonical_write_counts = _registry_write_capability_counts(product_workflow)
    if canonical_write_counts != EXPECTED_CANONICAL_WRITE_COUNTS:
        failures.append(
            f"{CANONICAL_WORKFLOW}: expected exact registry write primitives "
            f"{EXPECTED_CANONICAL_WRITE_COUNTS}, got {canonical_write_counts}"
        )

    resolver = repository_sources.get(RESOLVER)
    resolver_required = (
        'image_repository="ghcr.io/awakenworks/awaken-sandbox"',
        'docker buildx imagetools inspect "$release_tag"',
        "resolve-manifest",
        'docker buildx imagetools inspect "$immutable_image"',
        "validate-image-labels",
        "manifest unknown|name unknown|no such manifest",
        "printf 'present %s\\n' \"$immutable_image\"",
        "echo absent",
    )
    if resolver is None:
        failures.append(f"{RESOLVER}: missing sole registry resolver")
    else:
        for marker in resolver_required:
            if marker not in resolver:
                failures.append(f"{RESOLVER}: missing {marker!r}")
        if resolver.count("docker buildx imagetools inspect") != 2:
            failures.append(f"{RESOLVER}: expected exactly two digest-bound inspections")
        if "|not found" in resolver:
            failures.append(f"{RESOLVER}: generic not-found errors must fail closed")
    return failures


def _registry_write_capabilities(source: str) -> tuple[str, ...]:
    return tuple(_registry_write_capability_counts(source))


def _registry_write_capability_counts(source: str) -> dict[str, int]:
    capabilities = {
        name: len(tuple(pattern.finditer(source)))
        for name, pattern in REGISTRY_WRITE_PATTERNS
        if pattern.search(source)
    }
    if _has_oci_distribution_http_write(source):
        capabilities["OCI Distribution API write"] = 1
    return capabilities


def _has_oci_distribution_http_write(source: str) -> bool:
    """Bind a mutating method and Registry path within one source statement."""
    return any(
        OCI_DISTRIBUTION_PATH_RE.search(statement)
        and OCI_DISTRIBUTION_WRITE_METHOD_RE.search(statement)
        for statement in re.split(r";|\n[ \t]*\n", source)
    )


def _repository_sources() -> dict[str, str]:
    """Read every tracked executable/config source once for writer inventory."""
    sources: dict[str, str] = {}
    tracked = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
    )
    source_suffixes = {
        ".bash",
        ".cjs",
        ".go",
        ".js",
        ".json",
        ".lua",
        ".mjs",
        ".py",
        ".rs",
        ".sh",
        ".toml",
        ".ts",
        ".tsx",
        ".yaml",
        ".yml",
    }
    source_names = {"Dockerfile", "Justfile", "Makefile"}
    for raw_path in tracked.stdout.decode("utf-8").split("\0"):
        if not raw_path or raw_path == "scripts/ci/check_sandbox_image_release.py":
            continue
        path = REPO_ROOT / raw_path
        if path.suffix not in source_suffixes and path.name not in source_names:
            try:
                if not path.read_bytes().startswith(b"#!"):
                    continue
            except OSError:
                continue
        try:
            sources[raw_path] = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return sources


def resolver_self_test() -> list[str]:
    """Exercise the sole registry adapter with a deterministic fake Docker CLI."""
    failures: list[str] = []
    resolver = REPO_ROOT / RESOLVER
    if not resolver.is_file() or not os.access(resolver, os.X_OK):
        return [f"{RESOLVER}: must be an executable regular file"]

    # Resolver cause/effect graph and decision table:
    # exact tag manifest -> immutable digest labels -> exact identity -> present;
    # MANIFEST_UNKNOWN/NAME_UNKNOWN/no-such-manifest -> absent;
    # auth/generic missing-blob, malformed manifest, immutable read failure, or
    # label drift -> non-zero refusal without reporting absence/presence.
    digest = "a" * 64
    revision = "b" * 40
    fake_docker = f"""#!/usr/bin/env bash
set -euo pipefail
scenario="${{FAKE_DOCKER_SCENARIO:?}}"
reference="${{4:?}}"
if [[ "$reference" != *@* ]]; then
  case "$scenario" in
    manifest_unknown) echo 'manifest unknown' >&2; exit 1 ;;
    name_unknown) echo 'name unknown' >&2; exit 1 ;;
    no_such_manifest) echo 'no such manifest' >&2; exit 1 ;;
    auth) echo 'unauthorized: access denied' >&2; exit 1 ;;
    generic_not_found) echo 'config blob not found: not found' >&2; exit 1 ;;
    malformed) echo '{{'; exit 0 ;;
    *) printf '{{"digest":"sha256:{digest}"}}\\n'; exit 0 ;;
  esac
fi
if [[ "$scenario" == config_failure ]]; then
  echo 'immutable config read failed' >&2
  exit 1
fi
config_revision="{revision}"
[[ "$scenario" != drift ]] || config_revision="{'c' * 40}"
printf '{{"org.opencontainers.image.revision":"%s","org.opencontainers.image.source":"https://github.com/awakenworks/awaken","org.opencontainers.image.version":"v1.2.3"}}\\n' "$config_revision"
"""
    cases = (
        ("exact_present", "present", 0),
        ("manifest_unknown", "absent", 0),
        ("name_unknown", "absent", 0),
        ("no_such_manifest", "absent", 0),
        ("auth", None, 1),
        ("generic_not_found", None, 1),
        ("malformed", None, 1),
        ("config_failure", None, 1),
        ("drift", None, 1),
    )
    with tempfile.TemporaryDirectory(prefix="awaken-sandbox-release-resolver-") as raw_tmp:
        tmp = Path(raw_tmp)
        fake_bin = tmp / "bin"
        fake_bin.mkdir()
        docker = fake_bin / "docker"
        docker.write_text(fake_docker, encoding="utf-8")
        docker.chmod(0o755)
        for scenario, expected_kind, expected_zero in cases:
            env = os.environ.copy()
            env["FAKE_DOCKER_SCENARIO"] = scenario
            env["PATH"] = f"{fake_bin}:{env['PATH']}"
            result = subprocess.run(
                [
                    str(resolver),
                    f"{IMAGE_REPOSITORY}:v1.2.3",
                    revision,
                    "refs/tags/v1.2.3",
                    str(tmp / scenario),
                ],
                check=False,
                capture_output=True,
                text=True,
                env=env,
            )
            succeeded = result.returncode == 0
            if succeeded != (expected_zero == 0):
                failures.append(
                    f"resolver {scenario}: unexpected exit {result.returncode}: "
                    f"{result.stderr.strip()}"
                )
                continue
            if expected_kind == "absent" and result.stdout.strip() != "absent":
                failures.append(f"resolver {scenario}: expected exact absent projection")
            if expected_kind == "present" and result.stdout.strip() != (
                f"present {IMAGE_REPOSITORY}@sha256:{digest}"
            ):
                failures.append(
                    f"resolver {scenario}: expected exact immutable projection, got "
                    f"{result.stdout.strip()!r}: {result.stderr.strip()}"
                )
    return failures


def _swap_once(source: str, first: str, second: str) -> str:
    placeholder = "__AWAKEN_RELEASE_CHECKER_SWAP__"
    if source.count(first) != 1 or source.count(second) != 1 or placeholder in source:
        raise AssertionError("self-test swap markers must be unique")
    return source.replace(first, placeholder, 1).replace(second, first, 1).replace(
        placeholder, second, 1
    )


def self_test(
    workflow: str, build_script: str, repository_sources: dict[str, str]
) -> list[str]:
    failures = resolver_self_test()
    baseline_sources = repository_sources | {CANONICAL_WORKFLOW: workflow}
    if validate(workflow, build_script, baseline_sources):
        failures.append("baseline release workflow must satisfy its own checker")

    lookup_after_build = _swap_once(
        workflow,
        'initial="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
        CANONICAL_BUILD,
    )
    final_proof_block = """            verify_release_proof "$IMAGE" \\
              "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" \\
              staging-new
"""
    promotion_block = """              docker buildx imagetools create \\
                --prefer-index=false \\
                --tag "$RELEASE_TAG" \\
                --metadata-file "$RUNNER_TEMP/awaken-sandbox-promotion-metadata.json" \\
                "$IMAGE"
"""
    promotion_before_proof = _swap_once(workflow, final_proof_block, promotion_block)
    existing_proof_block = """              verify_release_proof "$IMAGE" \\
                "$RUNNER_TEMP/existing-awaken-sandbox-image-attestations.raw.json" \\
                existing
"""
    unsigned_preseed_bypass = workflow.replace(
        existing_proof_block, "            : # untrusted preseed accepted\n", 1
    )
    raw_before_verification = _swap_once(
        workflow,
        "            cosign verify-attestation \\",
        '              --attestations "$raw_attestations" \\',
    )

    # The mutations below are the executable rules from validate's decision table.
    workflow_cases = (
        ("missing package permission", workflow.replace("packages: write", "packages: read")),
        (
            "extra action permission",
            workflow.replace("id-token: write", "id-token: write\n      actions: write"),
        ),
        (
            "unprotected ref",
            workflow.replace("github.ref_protected == true", "github.ref_protected == false"),
        ),
        (
            "non-serialized release identity",
            workflow.replace(
                "group: release-awaken-${{ github.ref }}",
                "group: release-awaken-${{ github.run_id }}",
            ),
        ),
        (
            "cancelling release concurrency",
            workflow.replace("cancel-in-progress: false", "cancel-in-progress: true"),
        ),
        (
            "missing canonical staging build owner",
            workflow.replace(CANONICAL_BUILD, 'docker build -t "$STAGING_TAG" .'),
        ),
        ("mutable action", workflow.replace(ACTION_PINS[0], "actions/checkout@v6")),
        (
            "wrong image repository",
            workflow.replace(IMAGE_REPOSITORY, "ghcr.io/example/parallel-sandbox"),
        ),
        (
            "wrong source repository",
            workflow.replace(SOURCE_REPOSITORY, "https://github.com/example/spoof"),
        ),
        (
            "wrong predicate type",
            workflow.replace(PREDICATE_TYPE, "https://example.invalid/provenance"),
        ),
        (
            "identity regexp",
            workflow.replace("--certificate-identity ", "--certificate-identity-regexp "),
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
            "unsigned preseed accepted",
            workflow.replace('test -s "$raw_attestations"', ": # missing proof accepted"),
        ),
        ("existing proof bypassed", unsigned_preseed_bypass),
        (
            "existing image signature bypassed",
            workflow.replace(
                '              test "$SIGNATURE_STATE" = reuse\n'
                '              download_release_attestations "$IMAGE"',
                "              : # existing signature was not admitted\n"
                '              download_release_attestations "$IMAGE"',
                1,
            ),
        ),
        ("raw predicate selected before verification", raw_before_verification),
        (
            "staged predicate substitutes for an image signature",
            workflow.replace(
                'if [[ -s "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" ]]; then\n'
                '            test "$SIGNATURE_STATE" = reuse',
                'if [[ -s "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" ]]; then\n'
                "            : # predicate presence incorrectly substitutes for S1",
                1,
            ),
        ),
        (
            "crash-after-sign retry signs an existing signature again",
            workflow.replace(
                SIGNATURE_CREATE_BLOCK,
                '            cosign sign --yes "$IMAGE"\n'
                '            query_release_signature "$IMAGE" staging-after-sign\n'
                '            test "$SIGNATURE_STATE" = reuse\n',
                1,
            ),
        ),
        (
            "signature write is not followed by an exact requery",
            workflow.replace(
                '                query_release_signature "$IMAGE" staging-after-sign',
                "                : # missing post-sign registry requery",
                1,
            ),
        ),
        (
            "signature classifier is bypassed",
            workflow.replace(
                "publisher-signature-state \\",
                "emit-absence-without-classification \\",
                1,
            ),
        ),
        (
            "failed-job rerun repeats attestation",
            workflow.replace(
                'if [[ -s "$RUNNER_TEMP/staging-awaken-sandbox-image-attestations.raw.json" ]]; then',
                "if false; then",
            ),
        ),
        (
            "fresh subject treats no-attestation as a fatal download",
            workflow.replace(
                "if ! cosign download attestation \\",
                "cosign download attestation \\",
                1,
            ),
        ),
        (
            "attestation download failure is treated as absence",
            workflow.replace(
                "grep -Fqi 'no attestations' \"$download_error\"",
                "grep -Fqi 'error' \"$download_error\"",
                1,
            ),
        ),
        (
            "wrong workflow SHA proof",
            workflow.replace(
                '--certificate-github-workflow-sha "$SOURCE_REVISION"',
                '--certificate-github-workflow-sha "$GITHUB_RUN_ID"',
            ),
        ),
        (
            "wrong workflow trigger proof",
            workflow.replace(
                "--certificate-github-workflow-trigger push",
                "--certificate-github-workflow-trigger pull_request",
            ),
        ),
        ("build before semver resolution", lookup_after_build),
        ("promotion before exact proof", promotion_before_proof),
        (
            "staging registry identity is not rebound",
            workflow.replace(
                'staging="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
                'staging="present ${IMAGE_REPOSITORY}@sha256:${SOURCE_REVISION} #',
                1,
            ),
        ),
        (
            "promotion skips stable-tag compare",
            workflow.replace(
                'before="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
                'before="absent #',
                1,
            ),
        ),
        (
            "promotion skips final stable-tag closure",
            workflow.replace(
                'after="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
                'after="present $IMAGE #',
                1,
            ),
        ),
        (
            "existing reuse skips its final stable-tag closure",
            workflow.replace(
                'final_existing="$(scripts/release/resolve_awaken_sandbox_release_image.sh',
                'final_existing="present $IMAGE #',
                1,
            ),
        ),
        (
            "missing digest-preserving promotion",
            workflow.replace("--prefer-index=false", "--prefer-index=true"),
        ),
        (
            "promotion publishes a second stable tag",
            workflow.replace(
                '                --tag "$RELEASE_TAG" \\\n',
                '                --tag "$RELEASE_TAG" \\\n'
                '                --tag "${IMAGE_REPOSITORY}:stable" \\\n',
            ),
        ),
        (
            "promotion uses a short second-tag alias",
            workflow.replace(
                '                --tag "$RELEASE_TAG" \\\n',
                '                --tag "$RELEASE_TAG" \\\n'
                '                -t "${IMAGE_REPOSITORY}:stable" \\\n',
            ),
        ),
        (
            "promotion uses an equals second-tag alias",
            workflow.replace(
                '                --tag "$RELEASE_TAG" \\\n',
                '                --tag "$RELEASE_TAG" \\\n'
                '                --tag="${IMAGE_REPOSITORY}:stable" \\\n',
            ),
        ),
        (
            "promotion appends a short tag after the source",
            workflow.replace(
                '                "$IMAGE"\n'
                "              python3 scripts/release/awaken_sandbox_image_provenance.py \\",
                '                "$IMAGE" \\\n'
                '                -t "${IMAGE_REPOSITORY}:stable"\n'
                "              python3 scripts/release/awaken_sandbox_image_provenance.py \\",
            ),
        ),
        (
            "promotion digest unchecked",
            workflow.replace("validate-promotion", ": # promotion digest unchecked"),
        ),
        (
            "release tag pushed directly",
            workflow.replace('docker push "$STAGING_TAG"', 'docker push "$RELEASE_TAG"'),
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
    for name, candidate in workflow_cases:
        if not validate(
            candidate,
            build_script,
            baseline_sources | {CANONICAL_WORKFLOW: candidate},
        ):
            failures.append(f"{name}: expected checker refusal")

    build_cases = (
        (
            "missing source OCI label",
            build_script.replace('--label "org.opencontainers.image.source=$source"', ""),
        ),
        (
            "partial OCI label set accepted",
            build_script.replace(
                "if [[ -z $source || -z $revision || -z $version ]]; then",
                "if false; then",
            ),
        ),
        (
            "one build branch omits release labels",
            build_script.replace('"${release_image_labels[@]}"', "", 1),
        ),
    )
    for name, candidate in build_cases:
        if not validate(workflow, candidate, baseline_sources):
            failures.append(f"{name}: expected checker refusal")

    # Registry-owner cause/effect table:
    # a second workflow/local writer or another primitive in the canonical
    # workflow -> competing release authority -> reject before publication.
    writer_bypasses = (
        (
            "parallel publisher action",
            ".github/workflows/parallel-release.yml",
            "uses: example/publish-registry@deadbeef",
        ),
        ("ORAS publisher", "scripts/ops/publish.sh", "oras push image artifact"),
        ("ORAS copy alias", "scripts/ops/cp.sh", "oras cp source target"),
        (
            "ORAS manifest publisher",
            "scripts/ops/manifest.sh",
            "oras manifest push image manifest.json",
        ),
        (
            "ORAS blob publisher",
            "scripts/ops/blob.sh",
            "oras blob push image layer.tar",
        ),
        (
            "Cosign attachment publisher",
            "scripts/ops/attach.sh",
            "cosign attach sbom --sbom sbom.spdx.json image",
        ),
        (
            "Docker image publisher",
            "scripts/ops/docker-push.sh",
            "docker image push image",
        ),
        (
            "Buildx registry output",
            "scripts/ops/buildx-output.sh",
            "docker buildx build --output=type=registry .",
        ),
        (
            "Buildx short registry output",
            "scripts/ops/buildx-short-output.sh",
            "docker buildx build -o type=registry .",
        ),
        (
            "Buildx image output with push",
            "scripts/ops/buildx-image-output.sh",
            "docker buildx build --output type=image,push=true .",
        ),
        (
            "Buildx short image output with push",
            "scripts/ops/buildx-short-image-output.sh",
            "docker buildx build -o type=image,push=true .",
        ),
        ("Crane publisher", "scripts/ops/copy.sh", "crane copy source target"),
        ("Skopeo publisher", "scripts/ops/mirror.sh", "skopeo copy source target"),
        (
            "Regctl publisher",
            "scripts/ops/tag.sh",
            "regctl image copy source target",
        ),
        (
            "direct Registry API writer",
            "scripts/ops/http.py",
            'url = "/v2/awakenworks/awaken-sandbox/manifests/stable"\n'
            'method = "PUT"',
        ),
        (
            "Registry blob upload starter",
            "scripts/ops/post.py",
            'url = "/v2/awakenworks/awaken-sandbox/blobs/uploads/"\n'
            'method = "POST"',
        ),
        (
            "Registry chunk uploader",
            "scripts/ops/patch.py",
            'url = "/v2/awakenworks/awaken-sandbox/blobs/uploads/id"\n'
            'method = "PATCH"',
        ),
    )
    for name, path, source in writer_bypasses:
        if not validate(workflow, build_script, baseline_sources | {path: source}):
            failures.append(f"{name}: expected repository inventory refusal")

    unrelated_registry_read_and_post = (
        'await fetch(`${registry}/v2/`, { method: "GET" });\n'
        'await fetch("/api/runs", { method: "POST" });\n'
    )
    if validate(
        workflow,
        build_script,
        baseline_sources
        | {"scripts/ops/read-registry-and-post-api.ts": unrelated_registry_read_and_post},
    ):
        failures.append(
            "unrelated Registry read and API POST: expected non-writer projection"
        )

    same_workflow_bypasses = (
        ("same-workflow ORAS publisher", "          oras push extra-image artifact\n"),
        ("same-workflow ORAS copy alias", "          oras cp source target\n"),
        (
            "same-workflow Cosign attachment publisher",
            "          cosign attach sbom --sbom sbom.spdx.json image\n",
        ),
        ("same-workflow Docker publisher", '          docker push "$IMAGE"\n'),
        (
            "same-workflow Docker image publisher",
            '          docker image push "$IMAGE"\n',
        ),
        (
            "same-workflow Buildx registry output",
            "          docker buildx build --output=type=registry .\n",
        ),
        (
            "same-workflow direct Registry API writer",
            "          curl -X PUT https://ghcr.io/v2/awakenworks/"
            "awaken-sandbox/manifests/stable\n",
        ),
        (
            "same-workflow Registry blob upload starter",
            "          curl -X POST https://ghcr.io/v2/awakenworks/"
            "awaken-sandbox/blobs/uploads/\n",
        ),
        (
            "same-workflow Registry chunk uploader",
            "          curl -X PATCH https://ghcr.io/v2/awakenworks/"
            "awaken-sandbox/blobs/uploads/id\n",
        ),
        (
            "same-workflow second publisher action",
            "      - name: Parallel publisher\n"
            "        uses: example/publish-registry@deadbeef\n",
        ),
    )
    for name, writer in same_workflow_bypasses:
        candidate = workflow.replace(
            "      - name: Reuse a proven release or prove staging before promotion",
            writer
            + "      - name: Reuse a proven release or prove staging before promotion",
            1,
        )
        if not validate(
            candidate,
            build_script,
            baseline_sources | {CANONICAL_WORKFLOW: candidate},
        ):
            failures.append(f"{name}: expected exact primitive-count refusal")

    resolver_source = repository_sources.get(RESOLVER, "")
    drifted_resolver_sources = baseline_sources | {
        RESOLVER: resolver_source.replace(
            'docker buildx imagetools inspect "$immutable_image"',
            'docker buildx imagetools inspect "$release_tag"',
        )
    }
    if not validate(workflow, build_script, drifted_resolver_sources):
        failures.append("tag/config split resolver: expected checker refusal")
    return failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if not WORKFLOW.is_file() or not BUILD_SCRIPT.is_file():
        print("Awaken Sandbox image release contract violated:", file=sys.stderr)
        if not WORKFLOW.is_file():
            print("  .github/workflows/release.yml: missing sole publisher", file=sys.stderr)
        if not BUILD_SCRIPT.is_file():
            print("  deploy/images/sandbox/build.sh: missing canonical build", file=sys.stderr)
        return 1
    workflow = WORKFLOW.read_text(encoding="utf-8")
    build_script = BUILD_SCRIPT.read_text(encoding="utf-8")
    repository_sources = _repository_sources()
    if args.self_test:
        failures = self_test(workflow, build_script, repository_sources)
        if failures:
            print("Awaken Sandbox release checker self-test failed:", file=sys.stderr)
            for failure in failures:
                print(f"  {failure}", file=sys.stderr)
            return 2
        print("OK - Awaken Sandbox release checker self-test passed.")
        return 0

    failures = validate(workflow, build_script, repository_sources)
    if failures:
        print("Awaken Sandbox image release contract violated:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - one Open-owned prove-before-promotion release path is enforced.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
