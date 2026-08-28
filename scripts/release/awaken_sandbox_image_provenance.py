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
IN_TOTO_STATEMENT_V1 = "https://in-toto.io/Statement/v1"
DSSE_PAYLOAD_TYPE = "application/vnd.in-toto+json"
COSIGN_BUNDLE_MEDIA_TYPE = "application/vnd.dev.sigstore.bundle.v0.3+json"
COSIGN_SIGNATURE_PREDICATE_TYPE = "https://sigstore.dev/cosign/sign/v1"
LEGACY_COSIGN_SIGNATURE_TYPE = "cosign container image signature"
SOURCE_REPOSITORY = "https://github.com/awakenworks/awaken"
SOURCE_REPOSITORY_SLUG = "awakenworks/awaken"
IMAGE_REPOSITORY = "ghcr.io/awakenworks/awaken-sandbox"
WORKFLOW_PATH = ".github/workflows/release.yml"
MAX_ATTESTATIONS_BYTES = 4 * 1024 * 1024
MAX_ATTESTATION_COUNT = 64
MAX_STATEMENT_BYTES = 1024 * 1024
MAX_RELEASE_METADATA_BYTES = 1024 * 1024
MAX_SIGNATURE_BYTES = 4 * 1024 * 1024
MAX_SIGNATURE_COUNT = 64

REVISION_RE = re.compile(r"[0-9a-f]{40}")
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
RELEASE_REF_RE = re.compile(
    r"refs/tags/v(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
)
RUN_NUMBER_RE = re.compile(r"[1-9][0-9]*")
COSIGN_ABSENCE_TIME_RE = re.compile(
    r"[0-9]{4}/[0-9]{2}/[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}"
)


class ContractError(ValueError):
    """The signed provenance does not satisfy the Open-owned contract."""


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ContractError(f"JSON object: duplicate key {key!r}")
        value[key] = item
    return value


def _strict_json_decoder() -> json.JSONDecoder:
    return json.JSONDecoder(object_pairs_hook=_strict_object)


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
        value = json.loads(raw, object_pairs_hook=_strict_object)
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

    decoder = _strict_json_decoder()
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


def _read_bounded_input(path: Path, *, limit: int, location: str) -> bytes:
    with path.open("rb") as source:
        raw = source.read(limit + 1)
    if len(raw) > limit:
        raise ContractError(f"{location}: input exceeds the {limit}-byte bound")
    return raw


def _read_attestations(path: Path) -> bytes:
    return _read_bounded_input(
        path, limit=MAX_ATTESTATIONS_BYTES, location="attestations"
    )


def _read_release_metadata(path: Path) -> bytes:
    return _read_bounded_input(
        path, limit=MAX_RELEASE_METADATA_BYTES, location="release metadata"
    )


def _read_signature_input(path: Path, location: str) -> bytes:
    return _read_bounded_input(
        path, limit=MAX_SIGNATURE_BYTES, location=location
    )


def _statement_subject_digest(
    statement: dict[str, Any], location: str, *, expected_name: str | None = None
) -> str:
    subject = statement["subject"]
    if not isinstance(subject, list) or len(subject) != 1:
        raise ContractError(f"{location}.subject: expected one subject")
    subject_item = subject[0]
    if not isinstance(subject_item, dict):
        raise ContractError(f"{location}.subject[0]: expected an object")
    if expected_name is None:
        if not {"digest"}.issubset(subject_item) or not set(subject_item).issubset(
            {"annotations", "digest", "name"}
        ):
            raise ContractError(f"{location}.subject[0]: unexpected keys")
    else:
        _require_exact_keys(
            subject_item, {"digest", "name"}, f"{location}.subject[0]"
        )
        if subject_item["name"] != expected_name:
            raise ContractError(f"{location}.subject[0].name: unexpected image")
    digest = _require_exact_keys(
        subject_item["digest"], {"sha256"}, f"{location}.subject[0].digest"
    )
    return f"sha256:{_require_fullmatch(DIGEST_RE, digest['sha256'], location)}"


def _legacy_signature_record(record: dict[str, Any], location: str) -> tuple[str, str]:
    item = _require_exact_keys(
        record,
        {
            "Base64Signature",
            "Bundle",
            "Cert",
            "Chain",
            "Payload",
            "RFC3161Timestamp",
        },
        location,
    )
    signature = item["Base64Signature"]
    payload = item["Payload"]
    if not isinstance(signature, str) or not signature:
        raise ContractError(f"{location}.Base64Signature: expected base64 text")
    if not isinstance(payload, str) or not payload:
        raise ContractError(f"{location}.Payload: expected base64 text")
    try:
        base64.b64decode(signature, validate=True)
        payload_raw = base64.b64decode(payload, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ContractError(f"{location}: invalid base64 signature record") from error
    try:
        claim = json.loads(payload_raw.decode("utf-8"), object_pairs_hook=_strict_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{location}.Payload: invalid claim JSON") from error
    root = _require_exact_keys(claim, {"Critical", "Optional"}, f"{location}.claim")
    critical = _require_exact_keys(
        root["Critical"], {"Identity", "Image", "Type"}, f"{location}.claim.Critical"
    )
    identity = _require_exact_keys(
        critical["Identity"],
        {"docker-reference"},
        f"{location}.claim.Critical.Identity",
    )
    if not isinstance(identity["docker-reference"], str):
        raise ContractError(f"{location}.claim.Critical.Identity: invalid reference")
    if root["Optional"] is not None and not isinstance(root["Optional"], dict):
        raise ContractError(f"{location}.claim.Optional: invalid value")
    if critical["Type"] != LEGACY_COSIGN_SIGNATURE_TYPE:
        raise ContractError(f"{location}.claim.Critical.Type: unexpected value")
    image = _require_exact_keys(
        critical["Image"],
        {"Docker-manifest-digest"},
        f"{location}.claim.Critical.Image",
    )
    digest = image["Docker-manifest-digest"]
    if not isinstance(digest, str):
        raise ContractError(f"{location}.claim.Critical.Image: invalid digest")
    return "legacy", digest


def _signature_inventory(raw: bytes) -> tuple[list[tuple[str, str]], int]:
    if not raw:
        return [], 0
    if len(raw) > MAX_SIGNATURE_BYTES:
        raise ContractError("signatures: input exceeds the 4 MiB bound")
    if not raw.endswith(b"\n"):
        raise ContractError("signatures: JSONL must end with LF")
    records: list[tuple[str, str]] = []
    lines = raw.splitlines(keepends=True)
    if len(lines) > MAX_SIGNATURE_COUNT:
        raise ContractError("signatures: too many registry records")
    for index, line in enumerate(lines):
        if line == b"\n" or not line.endswith(b"\n"):
            raise ContractError(f"signatures[{index}]: blank or unterminated JSONL row")
        try:
            value = json.loads(
                line[:-1].decode("utf-8"), object_pairs_hook=_strict_object
            )
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ContractError(f"signatures[{index}]: invalid JSON") from error
        if not isinstance(value, dict):
            raise ContractError(f"signatures[{index}]: expected an object")
        item = value
        if "mediaType" in item:
            bundle = _require_exact_keys(
                item,
                {"dsseEnvelope", "mediaType", "verificationMaterial"},
                f"signatures[{index}]",
            )
            if bundle["mediaType"] != COSIGN_BUNDLE_MEDIA_TYPE:
                raise ContractError(f"signatures[{index}].mediaType: unexpected value")
            if not isinstance(bundle["verificationMaterial"], dict):
                raise ContractError(
                    f"signatures[{index}].verificationMaterial: expected an object"
                )
            statement = _decode_statement(
                bundle["dsseEnvelope"], f"signatures[{index}].dsseEnvelope"
            )
            if statement["predicateType"] == COSIGN_SIGNATURE_PREDICATE_TYPE:
                if statement["_type"] != IN_TOTO_STATEMENT_V1:
                    raise ContractError(
                        f"signatures[{index}].dsseEnvelope.statement._type: "
                        "unexpected value"
                    )
                if statement["predicate"] != {}:
                    raise ContractError(
                        f"signatures[{index}].dsseEnvelope.statement.predicate: "
                        "expected an empty object"
                    )
                records.append(
                    (
                        "bundle",
                        _statement_subject_digest(
                            statement, f"signatures[{index}].dsseEnvelope.statement"
                        ),
                    )
                )
            continue
        records.append(_legacy_signature_record(item, f"signatures[{index}]"))
    return records, len(lines)


def _verified_signature_inventory(raw: bytes) -> tuple[list[tuple[str, str]], int]:
    if len(raw) > MAX_SIGNATURE_BYTES:
        raise ContractError("verified signatures: input exceeds the 4 MiB bound")
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=_strict_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError("verified signatures: expected one JSON array") from error
    if not isinstance(value, list):
        raise ContractError("verified signatures: expected one JSON array")
    if len(value) > MAX_SIGNATURE_COUNT:
        raise ContractError("verified signatures: too many records")
    records: list[tuple[str, str]] = []
    for index, entry in enumerate(value):
        item = _require_exact_keys(
            entry, {"critical", "optional"}, f"verified signatures[{index}]"
        )
        critical = _require_exact_keys(
            item["critical"],
            {"identity", "image", "type"},
            f"verified signatures[{index}].critical",
        )
        identity = _require_exact_keys(
            critical["identity"],
            {"docker-reference"},
            f"verified signatures[{index}].critical.identity",
        )
        if not isinstance(identity["docker-reference"], str):
            raise ContractError(
                f"verified signatures[{index}].critical.identity: invalid reference"
            )
        if item["optional"] is not None and not isinstance(item["optional"], dict):
            raise ContractError(f"verified signatures[{index}].optional: invalid value")
        signature_type = critical["type"]
        if signature_type not in {
            COSIGN_SIGNATURE_PREDICATE_TYPE,
            LEGACY_COSIGN_SIGNATURE_TYPE,
        }:
            continue
        image = _require_exact_keys(
            critical["image"],
            {"docker-manifest-digest"},
            f"verified signatures[{index}].critical.image",
        )
        digest = image["docker-manifest-digest"]
        if not isinstance(digest, str):
            raise ContractError(
                f"verified signatures[{index}].critical.image: invalid digest"
            )
        records.append(
            (
                "bundle"
                if signature_type == COSIGN_SIGNATURE_PREDICATE_TYPE
                else "legacy",
                digest,
            )
        )
    return records, len(value)


def _validate_signature_absence(error_raw: bytes, expected_image: str) -> None:
    try:
        error = error_raw.decode("utf-8")
    except UnicodeDecodeError as decode_error:
        raise ContractError("signature query: stderr must be UTF-8") from decode_error
    expected = f"{expected_image}: no signatures associated"
    lines = error.splitlines(keepends=True)
    if len(lines) != 2 or any(not line.endswith("\n") for line in lines):
        raise ContractError("signature query: unexpected absence stderr shape")
    if lines[0] != f"Error: {expected}\n":
        raise ContractError("signature query: unexpected absence error")
    timed = lines[1].removesuffix("\n")
    timestamp, separator, message = timed.partition(" ")
    if not separator:
        raise ContractError("signature query: missing timestamp")
    # The Go logger emits date and time as two tokens. Split them together before
    # matching the pinned v3.0.6 error body.
    time_token, separator, message = message.partition(" ")
    if (
        not separator
        or COSIGN_ABSENCE_TIME_RE.fullmatch(f"{timestamp} {time_token}") is None
        or message != f"error during command execution: {expected}"
    ):
        raise ContractError("signature query: unexpected logged absence error")


def publisher_signature_state(
    *,
    expected_image: str,
    download_exit_code: int,
    signatures: bytes,
    verified_signatures: bytes,
    download_error: bytes,
) -> str:
    expected_digest = f"sha256:{_parse_image(expected_image)}"
    if download_exit_code == 1:
        if signatures or verified_signatures:
            raise ContractError("signature query: failed download produced output")
        _validate_signature_absence(download_error, expected_image)
        return "absent"
    if download_exit_code != 0:
        raise ContractError(
            f"signature query: unexpected download exit {download_exit_code}"
        )
    if download_error:
        raise ContractError("signature query: successful download wrote stderr")
    raw_records, raw_total = _signature_inventory(signatures)
    verified_records, verified_total = _verified_signature_inventory(
        verified_signatures
    )
    if not raw_records and not verified_records:
        if raw_total > 0 and verified_total > 0:
            return "absent"
        raise ContractError(
            "signature query: successful download returned an empty or unverified set"
        )
    if len(raw_records) != 1 or len(verified_records) != 1:
        raise ContractError(
            "signature query: expected exactly one raw and verified image signature"
        )
    if raw_records[0] != verified_records[0]:
        raise ContractError("signature query: raw and verified signatures disagree")
    if raw_records[0][1] != expected_digest:
        raise ContractError("signature query: signature binds another image digest")
    return "reuse"


def _decode_statement(envelope: Any, location: str) -> dict[str, Any]:
    item = _require_exact_keys(
        envelope, {"payload", "payloadType", "signatures"}, location
    )
    if item["payloadType"] != DSSE_PAYLOAD_TYPE:
        raise ContractError(f"{location}.payloadType: unexpected value")
    signatures = item["signatures"]
    if not isinstance(signatures, list) or not signatures:
        raise ContractError(f"{location}.signatures: expected a non-empty list")
    payload = item["payload"]
    if not isinstance(payload, str):
        raise ContractError(f"{location}.payload: expected base64 text")
    try:
        statement_raw = base64.b64decode(payload, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ContractError(f"{location}.payload: invalid base64") from error
    if len(statement_raw) > MAX_STATEMENT_BYTES:
        raise ContractError(f"{location}.payload: statement exceeds 1 MiB")
    try:
        statement = json.loads(
            statement_raw.decode("utf-8"), object_pairs_hook=_strict_object
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{location}.payload: invalid statement JSON") from error
    return _require_exact_keys(
        statement,
        {"_type", "predicate", "predicateType", "subject"},
        f"{location}.statement",
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
        statement = _decode_statement(envelope, f"attestations[{index}]")
        if statement["predicateType"] != PREDICATE_TYPE:
            continue
        if statement["_type"] != IN_TOTO_STATEMENT_TYPE:
            raise ContractError(
                f"attestations[{index}].statement._type: unexpected value"
            )
        digest = _statement_subject_digest(
            statement,
            f"attestations[{index}].statement",
            expected_name=IMAGE_REPOSITORY,
        )
        subject_image = f"{IMAGE_REPOSITORY}@{digest}"
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

    signature_statement = {
        "_type": "https://in-toto.io/Statement/v1",
        "predicate": {},
        "predicateType": COSIGN_SIGNATURE_PREDICATE_TYPE,
        "subject": [{"digest": {"sha256": "a" * 64}}],
    }
    signature_bundle = {
        "mediaType": COSIGN_BUNDLE_MEDIA_TYPE,
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payload": base64.b64encode(canonical_bytes(signature_statement)).decode(
                "ascii"
            ),
            "payloadType": DSSE_PAYLOAD_TYPE,
            "signatures": [{"sig": "self-test"}],
        },
    }
    verified_signature = {
        "critical": {
            "identity": {"docker-reference": image},
            "image": {"docker-manifest-digest": f"sha256:{'a' * 64}"},
            "type": COSIGN_SIGNATURE_PREDICATE_TYPE,
        },
        "optional": None,
    }
    signature_raw = (
        json.dumps(signature_bundle, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    verified_raw = json.dumps([verified_signature], separators=(",", ":")).encode(
        "utf-8"
    )
    absence_error = (
        f"Error: {image}: no signatures associated\n"
        f"2026/08/28 18:30:00 error during command execution: "
        f"{image}: no signatures associated\n"
    ).encode("utf-8")

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
    #
    # Image-signature cause/effect graph:
    # pinned-v3 exact-empty query -> S0; one raw image-signature record plus one
    # exact-identity verified record for the same digest -> S1; query/schema/count/
    # subject drift -> SX. Provenance P is independent and never substitutes for S.
    #
    # Decision table (the signature cases below own every rule):
    # raw S | verified S | exact absence stderr | effect
    #   0   |     0      | exact exit=1 pair    | absent
    #   0   |     0      | successful non-S P  | absent
    #   1   |     1      |          -           | reuse
    #  2+   |     *      |          -           | refuse ambiguity
    #   *   | 0/2+/drift |          -           | refuse mismatch
    #   0   |     0      | generic/error drift  | refuse query failure
    failures: list[str] = []

    def expect_signature_state(
        name: str,
        expected: str | None,
        *,
        exit_code: int,
        signatures: bytes,
        verified: bytes,
        error: bytes = b"",
    ) -> None:
        try:
            actual = publisher_signature_state(
                expected_image=image,
                download_exit_code=exit_code,
                signatures=signatures,
                verified_signatures=verified,
                download_error=error,
            )
        except ContractError as contract_error:
            if expected is not None:
                failures.append(f"{name}: unexpected refusal: {contract_error}")
            return
        if expected is None:
            failures.append(f"{name}: expected refusal")
        elif actual != expected:
            failures.append(f"{name}: expected {expected}, got {actual}")

    expect_signature_state(
        "exact pinned-v3 absence",
        "absent",
        exit_code=1,
        signatures=b"",
        verified=b"",
        error=absence_error,
    )
    expect_signature_state(
        "one exact bundle signature",
        "reuse",
        exit_code=0,
        signatures=signature_raw,
        verified=verified_raw,
    )
    for name, field, value in (
        ("signature statement type drift", "_type", IN_TOTO_STATEMENT_TYPE),
        ("signature predicate drift", "predicate", {"unexpected": True}),
    ):
        drifted_statement = copy.deepcopy(signature_statement)
        drifted_statement[field] = value
        drifted_bundle = copy.deepcopy(signature_bundle)
        drifted_bundle["dsseEnvelope"]["payload"] = base64.b64encode(
            canonical_bytes(drifted_statement)
        ).decode("ascii")
        expect_signature_state(
            name,
            None,
            exit_code=0,
            signatures=(
                json.dumps(drifted_bundle, separators=(",", ":")) + "\n"
            ).encode("utf-8"),
            verified=verified_raw,
        )

    provenance_statement = {
        "_type": "https://in-toto.io/Statement/v1",
        "predicate": {},
        "predicateType": PREDICATE_TYPE,
        "subject": [{"digest": {"sha256": "a" * 64}}],
    }
    provenance_bundle = copy.deepcopy(signature_bundle)
    provenance_bundle["dsseEnvelope"]["payload"] = base64.b64encode(
        canonical_bytes(provenance_statement)
    ).decode("ascii")
    verified_provenance = copy.deepcopy(verified_signature)
    verified_provenance["critical"]["type"] = PREDICATE_TYPE
    mixed_raw = signature_raw + (
        json.dumps(provenance_bundle, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    mixed_verified = json.dumps(
        [verified_signature, verified_provenance], separators=(",", ":")
    ).encode("utf-8")
    expect_signature_state(
        "signature beside independent provenance bundle",
        "reuse",
        exit_code=0,
        signatures=mixed_raw,
        verified=mixed_verified,
    )
    expect_signature_state(
        "provenance does not substitute for image signature",
        "absent",
        exit_code=0,
        signatures=(
            json.dumps(provenance_bundle, separators=(",", ":")) + "\n"
        ).encode("utf-8"),
        verified=json.dumps([verified_provenance], separators=(",", ":")).encode(
            "utf-8"
        ),
    )
    expect_signature_state(
        "successful empty download is not canonical absence",
        None,
        exit_code=0,
        signatures=b"",
        verified=b"[]",
    )
    expect_signature_state(
        "unverified non-signature bundle is not canonical absence",
        None,
        exit_code=0,
        signatures=(
            json.dumps(provenance_bundle, separators=(",", ":")) + "\n"
        ).encode("utf-8"),
        verified=b"[]",
    )

    legacy_claim = {
        "Critical": {
            "Identity": {"docker-reference": IMAGE_REPOSITORY},
            "Image": {"Docker-manifest-digest": f"sha256:{'a' * 64}"},
            "Type": LEGACY_COSIGN_SIGNATURE_TYPE,
        },
        "Optional": None,
    }
    legacy_record = {
        "Base64Signature": base64.b64encode(b"signature").decode("ascii"),
        "Payload": base64.b64encode(canonical_bytes(legacy_claim)).decode("ascii"),
        "Cert": None,
        "Chain": None,
        "Bundle": None,
        "RFC3161Timestamp": None,
    }
    verified_legacy = copy.deepcopy(verified_signature)
    verified_legacy["critical"]["type"] = LEGACY_COSIGN_SIGNATURE_TYPE
    expect_signature_state(
        "one exact legacy signature",
        "reuse",
        exit_code=0,
        signatures=(json.dumps(legacy_record, separators=(",", ":")) + "\n").encode(
            "utf-8"
        ),
        verified=json.dumps([verified_legacy], separators=(",", ":")).encode(
            "utf-8"
        ),
    )
    expect_signature_state(
        "duplicate raw image signatures",
        None,
        exit_code=0,
        signatures=signature_raw + signature_raw,
        verified=verified_raw,
    )
    expect_signature_state(
        "unbounded raw signature inventory",
        None,
        exit_code=0,
        signatures=signature_raw * (MAX_SIGNATURE_COUNT + 1),
        verified=verified_raw,
    )
    expect_signature_state(
        "oversized raw signature input",
        None,
        exit_code=0,
        signatures=b"x" * (MAX_SIGNATURE_BYTES + 1),
        verified=verified_raw,
    )
    expect_signature_state(
        "duplicate verified image signatures",
        None,
        exit_code=0,
        signatures=signature_raw,
        verified=json.dumps(
            [verified_signature, verified_signature], separators=(",", ":")
        ).encode("utf-8"),
    )
    wrong_verified = copy.deepcopy(verified_signature)
    wrong_verified["critical"]["image"]["docker-manifest-digest"] = (
        f"sha256:{'c' * 64}"
    )
    expect_signature_state(
        "verified signature subject drift",
        None,
        exit_code=0,
        signatures=signature_raw,
        verified=json.dumps([wrong_verified], separators=(",", ":")).encode("utf-8"),
    )
    for name, error in (
        (
            "generic no-signatures wording",
            b"Error: no signatures found\n"
            b"2026/08/28 18:30:00 error during command execution: "
            b"no signatures found\n",
        ),
        ("authorization failure", b"Error: unauthorized\n"),
        ("trailing third line", absence_error + b"usage drift\n"),
    ):
        expect_signature_state(
            name,
            None,
            exit_code=1,
            signatures=b"",
            verified=b"",
            error=error,
        )
    expect_signature_state(
        "successful download with stderr drift",
        None,
        exit_code=0,
        signatures=signature_raw,
        verified=verified_raw,
        error=b"warning: output contract drifted\n",
    )
    expect_signature_state(
        "noncanonical query exit",
        None,
        exit_code=2,
        signatures=b"",
        verified=b"",
        error=b"network failure\n",
    )
    expect_signature_state(
        "malformed signature JSONL",
        None,
        exit_code=0,
        signatures=b"{\n",
        verified=b"[]",
    )
    expect_signature_state(
        "duplicate signature JSON key",
        None,
        exit_code=0,
        signatures=b'{"mediaType":"a","mediaType":"b"}\n',
        verified=b"[]",
    )

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

    signature = subparsers.add_parser(
        "publisher-signature-state",
        help="classify the exact image signature as absent or reusable",
    )
    signature.add_argument("--image", required=True)
    signature.add_argument("--download-exit-code", required=True, type=int)
    signature.add_argument("--signatures", required=True, type=Path)
    signature.add_argument("--verified-signatures", required=True, type=Path)
    signature.add_argument("--download-error", required=True, type=Path)

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
        if args.command == "publisher-signature-state":
            print(
                publisher_signature_state(
                    expected_image=args.image,
                    download_exit_code=args.download_exit_code,
                    signatures=_read_signature_input(args.signatures, "signatures"),
                    verified_signatures=_read_signature_input(
                        args.verified_signatures, "verified signatures"
                    ),
                    download_error=_read_signature_input(
                        args.download_error, "signature query stderr"
                    ),
                )
            )
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
