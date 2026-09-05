#!/usr/bin/env python3
"""Generate and validate the canonical proof-carrying release manifest."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/ci"))
from formal_profile import profile_result
from formal_run_evidence import canonical_bytes, digest_bytes, digest_file, proof_toolchain, validate_record

HEX64 = re.compile(r"[0-9a-f]{64}")
PROFILES = {"standard", "awaken-verified-v1"}


class ContractError(ValueError):
    pass




def load_json(path: Path) -> dict:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ContractError(f"{path}: expected a JSON object")
    return value




def build_manifest(
    artifact: Path,
    *,
    profile: str,
    source_commit: str,
    repository: Path = ROOT,
    proof_run: dict | None = None,
) -> dict:
    if profile not in PROFILES:
        raise ContractError(f"unknown profile {profile!r}")
    if re.fullmatch(r"[0-9a-f]{40}", source_commit) is None:
        raise ContractError("source commit must be 40 lowercase hexadecimal characters")
    coverage = load_json(repository / "formal/coverage.json")
    features = load_json(repository / "formal/features.json")
    assumptions = load_json(repository / "formal/assumptions.json")
    boundaries = load_json(repository / "formal/proof-boundaries.json")
    result = profile_result(features, coverage)
    if profile == "awaken-verified-v1" and not result["eligible"]:
        raise ContractError(
            f"verified profile has {len(result['gaps'])} direct-proof gaps"
        )
    if proof_run is not None:
        try:
            validate_record(proof_run, repository)
        except ValueError as error:
            raise ContractError(str(error)) from error
        if proof_run["source"]["commit"] != source_commit:
            raise ContractError("formal evidence commit does not match the artifact source")
    elif profile == "awaken-verified-v1":
        raise ContractError("Verified packaging requires completed strict formal execution evidence")

    proof_paths = sorted(
        {
            relative
            for row in coverage["obligations"]
            for relative in row.get("evidence", [])
            if relative.startswith("formal/tla/")
            or row.get("proof_harnesses")
            or row["proof_grade"] == "implementation_refined"
        }
    )
    proof_inputs = [
        {"path": relative, "digest": digest_file(repository / relative)}
        for relative in proof_paths
    ]
    tcb = [
        {
            "id": item["id"],
            "contract_digest": digest_bytes(canonical_bytes(item)),
            "maximum_age_seconds": item["evidence_validity"][
                "maximum_age_seconds"
            ],
            "fail_closed_behavior": item["fail_closed_behavior"],
        }
        for item in assumptions["assumptions"]
        if item["id"] != "external.llm_semantics"
    ]
    return {
        "schema_version": 2,
        "profile": profile,
        "source_commit": source_commit,
        "artifact": {
            "name": artifact.name,
            "digest": digest_file(artifact),
        },
        "ledger_digests": {
            "coverage": digest_file(repository / "formal/coverage.json"),
            "features": digest_file(repository / "formal/features.json"),
            "assumptions": digest_file(repository / "formal/assumptions.json"),
            "proof_boundaries": digest_file(
                repository / "formal/proof-boundaries.json"
            ),
        },
        "profile_result": result,
        "proof_toolchain": proof_toolchain(repository),
        "proof_execution": proof_run,
        "proof_inputs_digest": digest_bytes(canonical_bytes(proof_inputs)),
        "proof_inputs": proof_inputs,
        "bounds": sorted(
            {
                bound
                for row in coverage["obligations"]
                for bound in row["bounds"]
            }
        ),
        "tcb_assumptions": tcb,
        "external_exclusions": [
            {
                "id": item["id"],
                "claim": item["dependency_contract"],
            }
            for item in assumptions["assumptions"]
            if item["id"] == "external.llm_semantics"
        ],
    }


def write_manifest(manifest: dict, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(canonical_bytes(manifest))


def validate_tcb_evidence(manifest: dict, evidence: dict, now: int) -> None:
    expected = {item["id"]: item for item in manifest["tcb_assumptions"]}
    observations = evidence.get("observations")
    if not isinstance(observations, list):
        raise ContractError("TCB evidence observations must be a list")
    actual = {item.get("id"): item for item in observations if isinstance(item, dict)}
    if set(actual) != set(expected) or len(actual) != len(observations):
        raise ContractError("TCB evidence must contain every assumption exactly once")
    for assumption_id, contract in expected.items():
        item = actual[assumption_id]
        if item.get("contract_digest") != contract["contract_digest"]:
            raise ContractError(f"{assumption_id}: contract digest mismatch")
        for field in ("version", "configuration_digest", "evidence_digest"):
            value = item.get(field)
            if not isinstance(value, str) or not value:
                raise ContractError(f"{assumption_id}: missing {field}")
        for field in ("configuration_digest", "evidence_digest"):
            if not item[field].startswith("sha256:") or HEX64.fullmatch(
                item[field].removeprefix("sha256:")
            ) is None:
                raise ContractError(f"{assumption_id}: invalid {field}")
        observed = item.get("observed_at_unix")
        if type(observed) is not int or observed > now:
            raise ContractError(f"{assumption_id}: invalid observation time")
        if now - observed > contract["maximum_age_seconds"]:
            raise ContractError(f"{assumption_id}: evidence expired")


def validate_manifest(
    manifest: dict,
    artifact: Path,
    *,
    repository: Path = ROOT,
    require_verified: bool = False,
    tcb_evidence: dict | None = None,
    now: int | None = None,
) -> None:
    if manifest.get("schema_version") != 2:
        raise ContractError("unsupported manifest schema")
    if manifest.get("profile") not in PROFILES:
        raise ContractError("unknown manifest profile")
    if manifest["artifact"]["name"] != artifact.name:
        raise ContractError("artifact name mismatch")
    if manifest["artifact"]["digest"] != digest_file(artifact):
        raise ContractError("artifact digest mismatch")
    expected = build_manifest(
        artifact,
        profile=manifest["profile"],
        source_commit=source_commit(repository),
        repository=repository,
        proof_run=manifest.get("proof_execution"),
    )
    if canonical_bytes(manifest) != canonical_bytes(expected):
        raise ContractError("manifest does not match the exact source, proof inputs, or ledgers")
    if require_verified:
        if (
            manifest["profile"] != "awaken-verified-v1"
            or manifest["profile_result"].get("eligible") is not True
        ):
            raise ContractError("artifact is not eligible for Awaken Verified Profile")
        if tcb_evidence is None:
            raise ContractError("Verified admission requires TCB evidence")
        validate_tcb_evidence(manifest, tcb_evidence, int(time.time()) if now is None else now)


class VerificationManifestTests(unittest.TestCase):
    # Cause/effect decision table: C1 profile is standard/Verified; C2 all P0
    # repository obligations have direct grades. E1 standard always reports the
    # exact gap set without a false badge; E2 Verified emits only for C2=true;
    # E3 Verified+C2=false fails. The current ledger exercises R1 and R3.
    def test_profile_generation_never_promotes_abstract_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            blocked = {
                "required_obligations": 2,
                "directly_proved_or_refined": 1,
                "eligible": False,
                "gaps": ["example.abstract_only"],
            }
            with patch(f"{__name__}.profile_result", return_value=blocked):
                standard = build_manifest(
                    artifact, profile="standard", source_commit="a" * 40
                )
                with self.assertRaises(ContractError):
                    build_manifest(
                        artifact,
                        profile="awaken-verified-v1",
                        source_commit="a" * 40,
                    )
            self.assertFalse(standard["profile_result"]["eligible"])
            eligible = dict(blocked, directly_proved_or_refined=2, eligible=True, gaps=[])
            with patch(f"{__name__}.profile_result", return_value=eligible):
                # V2: a green structural ledger without an executed gate must
                # fail. The old mocked-eligibility test manufactured a badge.
                with self.assertRaisesRegex(ContractError, "execution evidence"):
                    build_manifest(artifact, profile="awaken-verified-v1", source_commit="a" * 40)

    # Cause/effect graph: C1 artifact bytes match; C2 ledger digests match;
    # C3 Verified identity/eligibility match; C4 every exact TCB observation is
    # present, fingerprint-bound and fresh. E1 admission requires C1-C4; R1
    # standard validation covers C1/C2, R2 tampering negates C1, and R3 asks
    # for Verified without C3/C4 and must fail closed.
    def test_validation_decision_table(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            manifest = build_manifest(
                artifact, profile="standard", source_commit=source_commit(ROOT)
            )
            validate_manifest(manifest, artifact)
            artifact.write_bytes(b"tampered")
            with self.assertRaises(ContractError):
                validate_manifest(manifest, artifact)
            artifact.write_bytes(b"artifact")
            with self.assertRaises(ContractError):
                validate_manifest(manifest, artifact, require_verified=True)

    # Cause/effect decision table: C1 every TCB id occurs exactly once; C2 its
    # contract digest matches; C3 version/config/evidence fingerprints are
    # complete; C4 observation age is within the contract window. E1 accepts
    # only C1-C4. R1 is valid; R2 missing, R3 stale and R4 mismatched contract
    # independently negate the non-structural causes and all fail closed.
    def test_tcb_evidence_decision_table(self) -> None:
        manifest = {
            "tcb_assumptions": [
                {
                    "id": "database",
                    "contract_digest": f"sha256:{'1' * 64}",
                    "maximum_age_seconds": 60,
                }
            ]
        }
        observation = {
            "id": "database",
            "contract_digest": f"sha256:{'1' * 64}",
            "version": "17.1",
            "configuration_digest": f"sha256:{'2' * 64}",
            "evidence_digest": f"sha256:{'3' * 64}",
            "observed_at_unix": 100,
        }
        validate_tcb_evidence(manifest, {"observations": [observation]}, 150)
        with self.assertRaises(ContractError):
            validate_tcb_evidence(manifest, {"observations": []}, 150)
        with self.assertRaises(ContractError):
            validate_tcb_evidence(manifest, {"observations": [observation]}, 161)
        mismatched = dict(observation, contract_digest=f"sha256:{'4' * 64}")
        with self.assertRaises(ContractError):
            validate_tcb_evidence(manifest, {"observations": [mismatched]}, 150)


def source_commit(repository: Path) -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True
    ).strip()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    generate = subparsers.add_parser("generate")
    generate.add_argument("--artifact", type=Path, required=True)
    generate.add_argument("--output", type=Path, required=True)
    generate.add_argument("--profile", choices=sorted(PROFILES), default="standard")
    generate.add_argument("--proof-run", type=Path)
    validate = subparsers.add_parser("validate")
    validate.add_argument("--artifact", type=Path, required=True)
    validate.add_argument("--manifest", type=Path, required=True)
    validate.add_argument("--require-verified", action="store_true")
    validate.add_argument("--tcb-evidence", type=Path)
    subparsers.add_parser("self-test")
    args = parser.parse_args()

    if args.command == "self-test":
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(
            VerificationManifestTests
        )
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.command == "generate":
        manifest = build_manifest(
            args.artifact,
            profile=args.profile,
            source_commit=source_commit(ROOT),
            proof_run=load_json(args.proof_run) if args.proof_run else None,
        )
        write_manifest(manifest, args.output)
        print(digest_bytes(canonical_bytes(manifest)))
        return 0
    manifest = load_json(args.manifest)
    evidence = load_json(args.tcb_evidence) if args.tcb_evidence else None
    validate_manifest(
        manifest,
        args.artifact,
        require_verified=args.require_verified,
        tcb_evidence=evidence,
    )
    print(digest_bytes(canonical_bytes(manifest)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
