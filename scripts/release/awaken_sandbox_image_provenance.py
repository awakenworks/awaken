#!/usr/bin/env python3
"""Emit and validate the one Awaken Sandbox image provenance predicate."""

from __future__ import annotations

import argparse
import base64
import binascii
import copy
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
PREDICATE_TYPE = (
    "https://awakenworks.com/attestations/awaken-sandbox-image-provenance/v1"
)
# Pinned cosign v3.0.6 routes custom predicate URIs through its custom
# statement generator, which emits the legacy statement type below, and
# verify-attestation returns the verified signature payload as a DSSE envelope.
# A cosign upgrade must change this contract and its causal tests together;
# silently accepting another statement shape would hide supply-chain drift.
IN_TOTO_STATEMENT_TYPE = "https://in-toto.io/Statement/v0.1"
DSSE_PAYLOAD_TYPE = "application/vnd.in-toto+json"
SOURCE_REPOSITORY = "https://github.com/awakenworks/awaken"
SOURCE_REPOSITORY_SLUG = "awakenworks/awaken"
IMAGE_REPOSITORY = "ghcr.io/awakenworks/awaken-sandbox"
WORKFLOW_PATH = ".github/workflows/release.yml"
MAX_ATTESTATIONS_BYTES = 4 * 1024 * 1024
MAX_ATTESTATION_COUNT = 64
MAX_STATEMENT_BYTES = 1024 * 1024
MAX_RELEASE_METADATA_BYTES = 1024 * 1024

REVISION_RE = re.compile(r"[0-9a-f]{40}")
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
RELEASE_REF_RE = re.compile(
    r"refs/tags/v(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
)
RUN_NUMBER_RE = re.compile(r"[1-9][0-9]*")


class ContractError(ValueError):
    """The signed provenance does not satisfy the Open-owned contract."""


def _require_exact_keys(
    value: Any, expected: set[str], location: str
) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{location}: expected an object")
    actual = set(value)
    if actual != expected:
        raise ContractError(
            f"{location}: expected keys {sorted(expected)}, got {sorted(actual)}"
        )
    return value


def _require_fullmatch(
    pattern: re.Pattern[str], value: Any, location: str
) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise ContractError(f"{location}: invalid value")
    return value


def _parse_image(image: str) -> str:
    prefix = f"{IMAGE_REPOSITORY}@sha256:"
    if not image.startswith(prefix):
        raise ContractError(
            f"image: expected immutable {IMAGE_REPOSITORY}@sha256:<64 lowercase hex>"
        )
    digest = image[len(prefix) :]
    return _require_fullmatch(DIGEST_RE, digest, "image digest")


def _version_from_source_ref(source_ref: str) -> str:
    _require_fullmatch(RELEASE_REF_RE, source_ref, "source ref")
    return source_ref.removeprefix("refs/tags/")


def _workflow_identity(source_ref: str) -> str:
    return (
        f"https://github.com/{SOURCE_REPOSITORY_SLUG}/"
        f"{WORKFLOW_PATH}@{source_ref}"
    )


def _workflow_ref(source_ref: str) -> str:
    return f"{SOURCE_REPOSITORY_SLUG}/{WORKFLOW_PATH}@{source_ref}"


def build_predicate(
    *,
    image: str,
    revision: str,
    source_ref: str,
    workflow_ref: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, Any]:
    _parse_image(image)
    _require_fullmatch(REVISION_RE, revision, "source.revision")
    _require_fullmatch(RELEASE_REF_RE, source_ref, "source.ref")
    if workflow_ref != _workflow_ref(source_ref):
        raise ContractError(
            "builder workflow ref does not match the canonical release workflow/tag"
        )
    _require_fullmatch(RUN_NUMBER_RE, run_id, "builder.runId")
    _require_fullmatch(RUN_NUMBER_RE, run_attempt, "builder.runAttempt")
    return {
        "artifact": {"image": image},
        "builder": {
            "runAttempt": run_attempt,
            "runId": run_id,
            "workflow": _workflow_identity(source_ref),
        },
        "schemaVersion": SCHEMA_VERSION,
        "source": {
            "ref": source_ref,
            "repository": SOURCE_REPOSITORY,
            "revision": revision,
        },
    }


def validate_predicate(
    predicate: Any, *, expected_image: str, expected_revision: str
) -> dict[str, Any]:
    _parse_image(expected_image)
    _require_fullmatch(REVISION_RE, expected_revision, "expected revision")
    root = _require_exact_keys(
        predicate, {"artifact", "builder", "schemaVersion", "source"}, "predicate"
    )
    if type(root["schemaVersion"]) is not int or root["schemaVersion"] != SCHEMA_VERSION:
        raise ContractError(f"predicate.schemaVersion: expected {SCHEMA_VERSION}")

    artifact = _require_exact_keys(root["artifact"], {"image"}, "predicate.artifact")
    if artifact["image"] != expected_image:
        raise ContractError("predicate.artifact.image: does not match the requested image")

    source = _require_exact_keys(
        root["source"], {"ref", "repository", "revision"}, "predicate.source"
    )
    source_ref = _require_fullmatch(
        RELEASE_REF_RE, source["ref"], "predicate.source.ref"
    )
    if source["repository"] != SOURCE_REPOSITORY:
        raise ContractError("predicate.source.repository: unexpected repository")
    if source["revision"] != expected_revision:
        raise ContractError(
            "predicate.source.revision: does not match the requested revision"
        )

    builder = _require_exact_keys(
        root["builder"], {"runAttempt", "runId", "workflow"}, "predicate.builder"
    )
    if builder["workflow"] != _workflow_identity(source_ref):
        raise ContractError("predicate.builder.workflow: does not match source.ref")
    _require_fullmatch(RUN_NUMBER_RE, builder["runId"], "predicate.builder.runId")
    _require_fullmatch(
        RUN_NUMBER_RE, builder["runAttempt"], "predicate.builder.runAttempt"
    )
    return root


def canonical_bytes(predicate: dict[str, Any]) -> bytes:
    return (
        json.dumps(predicate, ensure_ascii=True, separators=(",", ":"), sort_keys=True)
        + "\n"
    ).encode("utf-8")


def predicate_digest(predicate: dict[str, Any]) -> str:
    return f"sha256:{hashlib.sha256(canonical_bytes(predicate)).hexdigest()}"


def _load_release_metadata(raw: bytes, location: str) -> dict[str, Any]:
    if len(raw) > MAX_RELEASE_METADATA_BYTES:
        raise ContractError(f"{location}: input exceeds the 1 MiB bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{location}: expected one UTF-8 JSON object") from error
    if not isinstance(value, dict):
        raise ContractError(f"{location}: expected an object")
    return value


def resolve_image_manifest(raw: bytes) -> str:
    manifest = _load_release_metadata(raw, "manifest")
    digest_value = manifest.get("digest")
    if not isinstance(digest_value, str) or not digest_value.startswith("sha256:"):
        raise ContractError("manifest.digest: expected sha256:<64 lowercase hex>")
    digest = _require_fullmatch(
        DIGEST_RE, digest_value.removeprefix("sha256:"), "manifest.digest"
    )
    return f"{IMAGE_REPOSITORY}@sha256:{digest}"


def validate_image_labels(
    raw: bytes, *, expected_revision: str, expected_source_ref: str
) -> None:
    labels = _load_release_metadata(raw, "image labels")
    _require_fullmatch(REVISION_RE, expected_revision, "expected revision")
    expected = {
        "org.opencontainers.image.source": SOURCE_REPOSITORY,
        "org.opencontainers.image.revision": expected_revision,
        "org.opencontainers.image.version": _version_from_source_ref(
            expected_source_ref
        ),
    }
    for name, value in expected.items():
        if labels.get(name) != value:
            raise ContractError(f"image labels.{name}: unexpected value")


def validate_promotion_metadata(raw: bytes, *, expected_image: str) -> None:
    expected_digest = _parse_image(expected_image)
    metadata = _load_release_metadata(raw, "promotion metadata")
    descriptor = metadata.get("containerimage.descriptor")
    if not isinstance(descriptor, dict):
        raise ContractError("promotion metadata: missing containerimage.descriptor")
    if descriptor.get("digest") != f"sha256:{expected_digest}":
        raise ContractError(
            "promotion metadata.containerimage.descriptor.digest: unexpected digest"
        )


def _load_json_stream(raw: bytes) -> list[Any]:
    if len(raw) > MAX_ATTESTATIONS_BYTES:
        raise ContractError("attestations: input exceeds the 4 MiB bound")
    try:
        decoded = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError("attestations: expected UTF-8 JSON") from error

    decoder = json.JSONDecoder()
    values: list[Any] = []
    index = 0
    while True:
        while index < len(decoded) and decoded[index].isspace():
            index += 1
        if index == len(decoded):
            break
        try:
            value, index = decoder.raw_decode(decoded, index)
        except json.JSONDecodeError as error:
            raise ContractError(
                f"attestations: invalid JSON at byte {error.pos}"
            ) from error
        if isinstance(value, list):
            values.extend(value)
        else:
            values.append(value)
        if len(values) > MAX_ATTESTATION_COUNT:
            raise ContractError("attestations: too many envelopes")
    if not values:
        raise ContractError("attestations: expected at least one DSSE envelope")
    return values


def _read_attestations(path: Path) -> bytes:
    with path.open("rb") as source:
        raw = source.read(MAX_ATTESTATIONS_BYTES + 1)
    if len(raw) > MAX_ATTESTATIONS_BYTES:
        raise ContractError("attestations: input exceeds the 4 MiB bound")
    return raw


def _read_release_metadata(path: Path) -> bytes:
    with path.open("rb") as source:
        raw = source.read(MAX_RELEASE_METADATA_BYTES + 1)
    if len(raw) > MAX_RELEASE_METADATA_BYTES:
        raise ContractError("release metadata: input exceeds the 1 MiB bound")
    return raw


def _decode_statement(envelope: Any, index: int) -> dict[str, Any]:
    item = _require_exact_keys(
        envelope, {"payload", "payloadType", "signatures"}, f"attestations[{index}]"
    )
    if item["payloadType"] != DSSE_PAYLOAD_TYPE:
        raise ContractError(f"attestations[{index}].payloadType: unexpected value")
    signatures = item["signatures"]
    if not isinstance(signatures, list) or not signatures:
        raise ContractError(
            f"attestations[{index}].signatures: expected a non-empty list"
        )
    payload = item["payload"]
    if not isinstance(payload, str):
        raise ContractError(f"attestations[{index}].payload: expected base64 text")
    try:
        statement_raw = base64.b64decode(payload, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ContractError(f"attestations[{index}].payload: invalid base64") from error
    if len(statement_raw) > MAX_STATEMENT_BYTES:
        raise ContractError(f"attestations[{index}].payload: statement exceeds 1 MiB")
    try:
        statement = json.loads(statement_raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(
            f"attestations[{index}].payload: invalid statement JSON"
        ) from error
    return _require_exact_keys(
        statement,
        {"_type", "predicate", "predicateType", "subject"},
        f"attestations[{index}].statement",
    )


def _matching_predicates(
    raw: bytes,
    *,
    expected_revision: str,
    expected_image: str,
    expected_source_ref: str,
) -> list[dict[str, Any]]:
    _parse_image(expected_image)
    _require_fullmatch(REVISION_RE, expected_revision, "expected revision")
    _require_fullmatch(RELEASE_REF_RE, expected_source_ref, "expected source ref")

    matches: list[dict[str, Any]] = []
    for index, envelope in enumerate(_load_json_stream(raw)):
        statement = _decode_statement(envelope, index)
        if statement["predicateType"] != PREDICATE_TYPE:
            continue
        if statement["_type"] != IN_TOTO_STATEMENT_TYPE:
            raise ContractError(
                f"attestations[{index}].statement._type: unexpected value"
            )
        subject = statement["subject"]
        if not isinstance(subject, list) or len(subject) != 1:
            raise ContractError(
                f"attestations[{index}].statement.subject: expected one subject"
            )
        subject_item = _require_exact_keys(
            subject[0],
            {"digest", "name"},
            f"attestations[{index}].statement.subject[0]",
        )
        if subject_item["name"] != IMAGE_REPOSITORY:
            raise ContractError(
                f"attestations[{index}].statement.subject[0].name: unexpected image"
            )
        subject_digest = _require_exact_keys(
            subject_item["digest"],
            {"sha256"},
            f"attestations[{index}].statement.subject[0].digest",
        )
        digest = _require_fullmatch(
            DIGEST_RE,
            subject_digest["sha256"],
            f"attestations[{index}].statement.subject[0].digest.sha256",
        )
        subject_image = f"{IMAGE_REPOSITORY}@sha256:{digest}"
        if subject_image != expected_image:
            raise ContractError(
                f"attestations[{index}].statement.subject[0].digest: unexpected digest"
            )
        predicate = validate_predicate(
            statement["predicate"],
            expected_image=subject_image,
            expected_revision=expected_revision,
        )
        if predicate["source"]["ref"] != expected_source_ref:
            raise ContractError(
                "predicate.source.ref: does not match the requested release identity"
            )
        matches.append(predicate)
    return matches


def validate_attestations(
    raw: bytes,
    *,
    expected_image: str,
    expected_revision: str,
    expected_source_ref: str,
) -> dict[str, Any]:
    matches = _matching_predicates(
        raw,
        expected_image=expected_image,
        expected_revision=expected_revision,
        expected_source_ref=expected_source_ref,
    )
    if len(matches) != 1:
        raise ContractError(
            "attestations: expected exactly one matching Awaken Sandbox provenance "
            f"predicate, got {len(matches)}"
        )
    return matches[0]


def _write_output(path: Path, predicate: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_bytes(predicate))


def _envelope(predicate: dict[str, Any], image: str) -> dict[str, Any]:
    digest = _parse_image(image)
    statement = {
        "_type": IN_TOTO_STATEMENT_TYPE,
        "predicate": predicate,
        "predicateType": PREDICATE_TYPE,
        "subject": [{"digest": {"sha256": digest}, "name": IMAGE_REPOSITORY}],
    }
    return {
        "payload": base64.b64encode(canonical_bytes(statement)).decode("ascii"),
        "payloadType": DSSE_PAYLOAD_TYPE,
        "signatures": [{"keyid": "", "sig": "self-test"}],
    }


def self_test() -> list[str]:
    image = f"{IMAGE_REPOSITORY}@sha256:{'a' * 64}"
    revision = "b" * 40
    source_ref = "refs/tags/v1.2.3"
    predicate = build_predicate(
        image=image,
        revision=revision,
        source_ref=source_ref,
        workflow_ref=_workflow_ref(source_ref),
        run_id="123",
        run_attempt="2",
    )
    valid = _envelope(predicate, image)

    # Cause/effect graph:
    # verified DSSE + exact subject + exact Open predicate -> canonical bytes/digest
    # wrong image/revision/ref/workflow/schema or zero/multiple matches -> refuse
    # malformed, format-drifted, or unbounded envelope -> refuse before acceptance
    #
    # Decision table (the cases below own every rule):
    # DSSE v0.1 | match count | subject | predicate | effect
    #    exact  |      1      | exact   | exact     | canonical success
    #    exact  |    0 or 2   |   *     |   *       | refuse absence/ambiguity
    #    exact  |      1      | wrong   |   *       | refuse artifact mismatch
    #    exact  |      1      | exact   | wrong     | refuse provenance mismatch
    # wrong/bad |      *      |   *     |   *       | refuse format/unbounded input
    #
    # Publisher-rerun cause/effect graph:
    # existing semver tag + exact workflow proof + exact labels -> reuse digest
    # absent semver tag -> staging digest may receive the one proof before promotion
    # unsigned/preseeded tag, missing/conflicting proof, or wrong labels -> refuse
    # promotion metadata must bind the already-verified immutable digest exactly
    #
    # Decision table (the release-selection cases below own every rule):
    # semver tag | staged proof | release proof | labels | count | effect
    #   absent   |       0      |       -       | exact  |   0   | prove/promote
    #   absent   |    1 exact   |       -       | exact  |   1   | reuse/promote
    #   absent   |  2+/conflict |       -       |   *    |  2+   | refuse
    #   present  |       -      |     exact     | exact  |   1   | reuse canonical
    #   present  |       -      | missing/wrong |   *    |   *   | refuse preseed
    #   present  |       -      |     exact     | wrong  |   *   | refuse drift
    #   present  |       -      |     exact     | exact  | 0/2+  | refuse ambiguity
    failures: list[str] = []

    def expect_success(name: str, value: bytes) -> None:
        try:
            actual = validate_attestations(
                value,
                expected_image=image,
                expected_revision=revision,
                expected_source_ref=source_ref,
            )
            if canonical_bytes(actual) != canonical_bytes(predicate):
                failures.append(f"{name}: canonical predicate drifted")
        except ContractError as error:
            failures.append(f"{name}: unexpected refusal: {error}")

    def expect_refusal(name: str, value: bytes) -> None:
        try:
            validate_attestations(
                value,
                expected_image=image,
                expected_revision=revision,
                expected_source_ref=source_ref,
            )
        except ContractError:
            return
        failures.append(f"{name}: expected refusal")

    encoded = json.dumps(valid, separators=(",", ":")).encode("utf-8")
    expect_success("exact envelope", encoded)
    expect_success("cosign JSON array", json.dumps([valid]).encode("utf-8"))
    expect_refusal(
        "duplicate matching envelope",
        f"{encoded.decode()}\n{encoded.decode()}\n".encode("utf-8"),
    )

    expect_refusal("zero release attestations", b"")
    selected = validate_attestations(
        encoded,
        expected_image=image,
        expected_revision=revision,
        expected_source_ref=source_ref,
    )
    if canonical_bytes(selected) != canonical_bytes(predicate):
        failures.append("exact release attestation: canonical predicate drifted")
    rerun_predicate = build_predicate(
        image=image,
        revision=revision,
        source_ref=source_ref,
        workflow_ref=_workflow_ref(source_ref),
        run_id="123",
        run_attempt="3",
    )
    if predicate_digest(selected) == predicate_digest(rerun_predicate):
        failures.append("failed-job rerun: original canonical digest was not retained")
    expect_refusal(
        "multiple release attestations",
        f"{encoded.decode()}\n{encoded.decode()}\n".encode("utf-8"),
    )

    conflicting_release = copy.deepcopy(predicate)
    conflicting_release["source"]["ref"] = "refs/tags/v1.2.4"
    conflicting_release["builder"]["workflow"] = _workflow_identity(
        "refs/tags/v1.2.4"
    )
    expect_refusal(
        "conflicting release identity",
        json.dumps(_envelope(conflicting_release, image)).encode("utf-8"),
    )

    labels = {
        "org.opencontainers.image.source": SOURCE_REPOSITORY,
        "org.opencontainers.image.revision": revision,
        "org.opencontainers.image.version": "v1.2.3",
        "org.awaken.environment-packages": "2",
    }
    try:
        validate_image_labels(
            json.dumps(labels).encode("utf-8"),
            expected_revision=revision,
            expected_source_ref=source_ref,
        )
    except ContractError as error:
        failures.append(f"exact OCI release labels: unexpected refusal: {error}")
    for name in (
        "org.opencontainers.image.source",
        "org.opencontainers.image.revision",
        "org.opencontainers.image.version",
    ):
        conflicting_labels = copy.deepcopy(labels)
        conflicting_labels[name] = "wrong"
        try:
            validate_image_labels(
                json.dumps(conflicting_labels).encode("utf-8"),
                expected_revision=revision,
                expected_source_ref=source_ref,
            )
        except ContractError:
            pass
        else:
            failures.append(f"conflicting OCI label {name}: expected refusal")

    manifest = json.dumps({"digest": f"sha256:{'a' * 64}"}).encode("utf-8")
    if resolve_image_manifest(manifest) != image:
        failures.append("immutable manifest resolution: image coordinate drifted")
    promotion = json.dumps(
        {"containerimage.descriptor": {"digest": f"sha256:{'a' * 64}"}}
    ).encode("utf-8")
    try:
        validate_promotion_metadata(promotion, expected_image=image)
    except ContractError as error:
        failures.append(f"exact promotion metadata: unexpected refusal: {error}")
    wrong_promotion = json.dumps(
        {"containerimage.descriptor": {"digest": f"sha256:{'c' * 64}"}}
    ).encode("utf-8")
    try:
        validate_promotion_metadata(wrong_promotion, expected_image=image)
    except ContractError:
        pass
    else:
        failures.append("different promoted digest: expected refusal")

    wrong_predicate_type = copy.deepcopy(valid)
    statement = json.loads(base64.b64decode(wrong_predicate_type["payload"]))
    statement["predicateType"] = "https://example.invalid/not-open-provenance"
    wrong_predicate_type["payload"] = base64.b64encode(
        canonical_bytes(statement)
    ).decode("ascii")
    expect_refusal(
        "zero matching envelopes", json.dumps(wrong_predicate_type).encode("utf-8")
    )

    wrong_subject = copy.deepcopy(valid)
    statement = json.loads(base64.b64decode(wrong_subject["payload"]))
    statement["subject"][0]["digest"]["sha256"] = "c" * 64
    wrong_subject["payload"] = base64.b64encode(canonical_bytes(statement)).decode(
        "ascii"
    )
    expect_refusal("wrong subject", json.dumps(wrong_subject).encode("utf-8"))

    for name, path, value in (
        ("wrong revision", ("source", "revision"), "c" * 40),
        (
            "wrong image",
            ("artifact", "image"),
            f"{IMAGE_REPOSITORY}@sha256:{'d' * 64}",
        ),
        ("wrong workflow", ("builder", "workflow"), "https://example.invalid/workflow"),
        ("branch ref", ("source", "ref"), "refs/heads/main"),
        ("leading-zero tag", ("source", "ref"), "refs/tags/v01.2.3"),
    ):
        mutated = copy.deepcopy(predicate)
        mutated[path[0]][path[1]] = value
        expect_refusal(name, json.dumps(_envelope(mutated, image)).encode("utf-8"))

    extra_key = copy.deepcopy(predicate)
    extra_key["parallelProvenance"] = True
    expect_refusal(
        "unknown predicate key", json.dumps(_envelope(extra_key, image)).encode("utf-8")
    )

    no_signature = copy.deepcopy(valid)
    no_signature["signatures"] = []
    expect_refusal("missing DSSE signature", json.dumps(no_signature).encode("utf-8"))

    extra_envelope_key = copy.deepcopy(valid)
    extra_envelope_key["verificationMaterial"] = {}
    expect_refusal(
        "unknown DSSE envelope key", json.dumps(extra_envelope_key).encode("utf-8")
    )

    statement_v1 = copy.deepcopy(valid)
    statement = json.loads(base64.b64decode(statement_v1["payload"]))
    statement["_type"] = "https://in-toto.io/Statement/v1"
    statement_v1["payload"] = base64.b64encode(canonical_bytes(statement)).decode(
        "ascii"
    )
    expect_refusal(
        "unproven in-toto v1 drift", json.dumps(statement_v1).encode("utf-8")
    )
    expect_refusal("malformed JSON", b"{")
    expect_refusal("unbounded input", b" " * (MAX_ATTESTATIONS_BYTES + 1))
    return failures


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    emit = subparsers.add_parser("emit", help="write one canonical release predicate")
    emit.add_argument("--image", required=True)
    emit.add_argument("--revision", required=True)
    emit.add_argument("--source-ref", required=True)
    emit.add_argument("--workflow-ref", required=True)
    emit.add_argument("--run-id", required=True)
    emit.add_argument("--run-attempt", required=True)
    emit.add_argument("--output", required=True, type=Path)

    context = subparsers.add_parser(
        "validate-context", help="fail before build when release context is not exact"
    )
    context.add_argument("--revision", required=True)
    context.add_argument("--source-ref", required=True)
    context.add_argument("--workflow-ref", required=True)
    context.add_argument("--run-id", required=True)
    context.add_argument("--run-attempt", required=True)

    validate = subparsers.add_parser(
        "validate-attestations", help="validate verified DSSE and canonicalize it"
    )
    validate.add_argument("--image", required=True)
    validate.add_argument("--revision", required=True)
    validate.add_argument("--source-ref", required=True)
    validate.add_argument("--attestations", required=True, type=Path)
    validate.add_argument("--output", required=True, type=Path)

    resolve = subparsers.add_parser(
        "resolve-manifest", help="resolve one registry manifest to its immutable image"
    )
    resolve.add_argument("--manifest", required=True, type=Path)

    labels = subparsers.add_parser(
        "validate-image-labels", help="validate exact OCI release identity labels"
    )
    labels.add_argument("--labels", required=True, type=Path)
    labels.add_argument("--revision", required=True)
    labels.add_argument("--source-ref", required=True)

    promotion = subparsers.add_parser(
        "validate-promotion", help="bind promotion metadata to the proven digest"
    )
    promotion.add_argument("--metadata", required=True, type=Path)
    promotion.add_argument("--image", required=True)

    subparsers.add_parser("self-test", help="run dependency-free causal tests")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        if args.command == "self-test":
            failures = self_test()
            if failures:
                print("Awaken Sandbox image provenance self-test failed:", file=sys.stderr)
                for failure in failures:
                    print(f"  {failure}", file=sys.stderr)
                return 2
            print("OK - Awaken Sandbox image provenance causal self-test passed.")
            return 0
        if args.command == "validate-context":
            build_predicate(
                image=f"{IMAGE_REPOSITORY}@sha256:{'0' * 64}",
                revision=args.revision,
                source_ref=args.source_ref,
                workflow_ref=args.workflow_ref,
                run_id=args.run_id,
                run_attempt=args.run_attempt,
            )
            print("OK - exact Awaken Sandbox image release context accepted.")
            return 0
        if args.command == "resolve-manifest":
            print(resolve_image_manifest(_read_release_metadata(args.manifest)))
            return 0
        if args.command == "validate-image-labels":
            validate_image_labels(
                _read_release_metadata(args.labels),
                expected_revision=args.revision,
                expected_source_ref=args.source_ref,
            )
            print("OK - exact OCI release identity labels accepted.")
            return 0
        if args.command == "validate-promotion":
            validate_promotion_metadata(
                _read_release_metadata(args.metadata), expected_image=args.image
            )
            print("OK - release tag promotion retained the proven digest.")
            return 0
        if args.command == "emit":
            predicate = build_predicate(
                image=args.image,
                revision=args.revision,
                source_ref=args.source_ref,
                workflow_ref=args.workflow_ref,
                run_id=args.run_id,
                run_attempt=args.run_attempt,
            )
        else:
            predicate = validate_attestations(
                _read_attestations(args.attestations),
                expected_image=args.image,
                expected_revision=args.revision,
                expected_source_ref=args.source_ref,
            )
        _write_output(args.output, predicate)
        print(predicate_digest(predicate))
        return 0
    except (ContractError, OSError) as error:
        print(f"Awaken Sandbox image provenance rejected: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
