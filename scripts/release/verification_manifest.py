#!/usr/bin/env python3
"""Generate and validate the canonical proof-carrying release manifest."""

from __future__ import annotations

import argparse
import copy
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


def _tcb_index(items: object, label: str) -> dict[str, dict]:
    """Index a deployment snapshot without silently collapsing duplicate IDs."""
    if not isinstance(items, list) or not items:
        raise ContractError(f"{label}: expected a nonempty list")
    indexed = {}
    for item in items:
        if not isinstance(item, dict):
            raise ContractError(f"{label}: expected an object")
        identity = item.get("id")
        if not isinstance(identity, str) or not identity or identity in indexed:
            raise ContractError(f"{label}: missing or duplicate assumption id")
        indexed[identity] = item
    return indexed


def _sha256(value: object) -> bool:
    return isinstance(value, str) and value.startswith("sha256:") and HEX64.fullmatch(value[7:]) is not None


def validate_tcb_evidence(
    manifest: dict, evidence: dict, now: int, *, runtime_fingerprint: dict | None = None,
) -> None:
    # One point-in-time admission boundary, not authentication or a renewable
    # runtime lease. The deployment caller must independently authenticate both
    # the observation and target snapshot; copying one into the other is unsafe.
    if type(now) is not int or now < 0:
        raise ContractError("TCB admission requires a nonnegative integer clock")
    if not isinstance(runtime_fingerprint, dict) or set(runtime_fingerprint) != {"dependencies"}:
        raise ContractError("Verified admission requires an exact target runtime fingerprint")
    if not isinstance(evidence, dict) or set(evidence) != {"observations"}:
        raise ContractError("TCB evidence requires exactly the observations field")
    expected = _tcb_index(manifest.get("tcb_assumptions"), "TCB contracts")
    actual = _tcb_index(evidence["observations"], "TCB observations")
    target = _tcb_index(runtime_fingerprint["dependencies"], "target runtime fingerprint")
    if set(actual) != set(expected) or set(target) != set(expected):
        raise ContractError("TCB evidence and target must contain every assumption exactly once")
    for assumption_id, contract in expected.items():
        item = actual[assumption_id]
        dependency = target[assumption_id]
        if set(dependency) != {"id", "version", "configuration_digest"} or set(item) != {
            "id", "contract_digest", "version", "configuration_digest", "evidence_digest", "observed_at_unix",
        }:
            raise ContractError(f"{assumption_id}: unknown or missing fingerprint fields")
        if not _sha256(contract.get("contract_digest")) or item["contract_digest"] != contract["contract_digest"]:
            raise ContractError(f"{assumption_id}: contract digest mismatch")
        for fingerprint in (item, dependency):
            version = fingerprint["version"]
            if not isinstance(version, str) or not version or version.strip() != version:
                raise ContractError(f"{assumption_id}: invalid version")
        for field in ("configuration_digest", "evidence_digest"):
            if not _sha256(item[field]):
                raise ContractError(f"{assumption_id}: invalid {field}")
        if any(item[field] != dependency[field] for field in ("version", "configuration_digest")):
            raise ContractError(f"{assumption_id}: target runtime fingerprint mismatch")
        maximum_age = contract.get("maximum_age_seconds")
        if type(maximum_age) is not int or maximum_age <= 0:
            raise ContractError(f"{assumption_id}: invalid maximum evidence age")
        observed = item["observed_at_unix"]
        if type(observed) is not int or not 0 <= observed <= now:
            raise ContractError(f"{assumption_id}: invalid observation time")
        if now - observed > maximum_age:
            raise ContractError(f"{assumption_id}: evidence expired")


def validate_manifest(
    manifest: dict,
    artifact: Path,
    *,
    repository: Path = ROOT,
    require_verified: bool = False,
    tcb_evidence: dict | None = None,
    runtime_fingerprint: dict | None = None,
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
        validate_tcb_evidence(
            manifest, tcb_evidence, int(time.time()) if now is None else now,
            runtime_fingerprint=runtime_fingerprint,
        )


class VerificationManifestTests(unittest.TestCase):
    @staticmethod
    def tcb_case() -> tuple[dict, dict, dict]:
        contracts, observations, dependencies = [], [], []
        for identity in ("database", "network"):
            contract = {"id": identity, "contract_digest": "sha256:" + "1" * 64, "maximum_age_seconds": 60}
            dependency = {"id": identity, "version": "17.1", "configuration_digest": "sha256:" + "2" * 64}
            contracts.append(contract)
            dependencies.append(dependency)
            observations.append(dict(dependency, contract_digest=contract["contract_digest"],
                                     evidence_digest="sha256:" + "3" * 64, observed_at_unix=100))
        return {"tcb_assumptions": contracts}, {"observations": observations}, {"dependencies": dependencies}

    def test_fresh_evidence_without_target_runtime_binding_is_rejected(self) -> None:
        # Cause/effect: a fresh observation has syntactically valid hashes but
        # no independently supplied target fingerprint. It must not qualify a
        # deployment; a version/config from another environment is not proof.
        manifest, evidence, _ = self.tcb_case()
        with self.assertRaises(ContractError):
            validate_tcb_evidence(manifest, evidence, 150)

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

    # Cause/effect graph: complete unique identities AND exact contract AND
    # valid evidence hash AND exact target (version AND configuration) AND
    # nonnegative observation <= clock <= observation + positive max age =>
    # accept. Otherwise reject without producing a Verified admission. Two
    # dependencies expose accidental "any" instead of "all" acceptance.
    # R1 accepts the inclusive age boundary and arbitrary list order. R2 checks
    # all four version/config combinations for either dependency. R3 negates
    # each structural, scalar and temporal cause independently, including bool
    # masquerading as int. Other causes remain valid to isolate each guard.
    def test_tcb_evidence_decision_table(self) -> None:
        manifest, evidence, target = self.tcb_case()
        target["dependencies"].reverse()
        for now in (100, 150, 160):
            validate_tcb_evidence(manifest, evidence, now, runtime_fingerprint=target)
        for index in range(2):
            for version_matches in (False, True):
                for config_matches in (False, True):
                    manifest, evidence, target = self.tcb_case()
                    target["dependencies"][index].update(
                        version="17.1" if version_matches else "18.0",
                        configuration_digest="sha256:" + ("2" if config_matches else "4") * 64,
                    )
                    with self.subTest(index=index, version=version_matches, config=config_matches):
                        if version_matches and config_matches:
                            validate_tcb_evidence(manifest, evidence, 150, runtime_fingerprint=target)
                        else:
                            with self.assertRaisesRegex(ContractError, "fingerprint mismatch"):
                                validate_tcb_evidence(manifest, evidence, 150, runtime_fingerprint=target)

        cases = []
        for label, root_key, list_key in (
            ("contracts", "manifest", "tcb_assumptions"),
            ("observations", "evidence", "observations"),
            ("target", "target", "dependencies"),
        ):
            for replacement in ([], None, {}, [None], [{}], [{"id": []}]):
                cases.append((label + " malformed list", (root_key, list_key), replacement))
            for field, value in (("id", "database"), ("id", "unknown"), ("id", ""), ("id", None)):
                cases.append((label + " identity", (root_key, list_key, 1, field), value))
        for root_key, list_key in (("evidence", "observations"), ("target", "dependencies")):
            for field, value in (("version", ""), ("version", "  "), ("version", 17),
                                 ("configuration_digest", "sha256:abc"), ("unknown", True)):
                cases.append(("fingerprint fields", (root_key, list_key, 1, field), value))
        for field, values in {
            "observed_at_unix": (-1, 151, True, 100.0, None),
            "contract_digest": ("sha256:" + "4" * 64, None),
            "evidence_digest": ("sha256:abc", "sha256:" + "A" * 64, None),
        }.items():
            for value in values:
                cases.append((field, ("evidence", "observations", 1, field), value))
        for value in (0, -1, True, 60.0, None):
            cases.append(("max age", ("manifest", "tcb_assumptions", 1, "maximum_age_seconds"), value))
        cases.append(("contract hash syntax", ("manifest", "tcb_assumptions", 1, "contract_digest"), "invalid"))
        for value in (-1, True, 150.0, None, 99, 161):
            cases.append(("clock or expiry", ("now",), value))
        for root_key in ("evidence", "target"):
            cases.append(("unknown envelope field", (root_key, "unknown"), True))
        for label, path, value in cases:
            manifest, evidence, target = self.tcb_case()
            case = dict(manifest=manifest, evidence=evidence, target=target, now=150)
            cursor = case
            for key in path[:-1]:
                cursor = cursor[key]
            cursor[path[-1]] = value
            with self.subTest(rule=label, path=path, value=value), self.assertRaises(ContractError):
                validate_tcb_evidence(case["manifest"], case["evidence"], case["now"],
                                      runtime_fingerprint=case["target"])

    def test_verified_manifest_forwards_target_to_canonical_admission(self) -> None:
        # R4: after exact release/eligibility validation, the target must reach
        # the same validator. Mock only the earlier proof/ledger gate, not TCB
        # admission: matching succeeds; absent/foreign target must still fail.
        manifest, evidence, target = self.tcb_case()
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            manifest.update(schema_version=2, profile="awaken-verified-v1", profile_result={"eligible": True},
                            artifact={"name": artifact.name, "digest": digest_file(artifact)})
            with patch(f"{__name__}.build_manifest", return_value=manifest):
                validate_manifest(manifest, artifact, require_verified=True, tcb_evidence=evidence,
                                  runtime_fingerprint=target, now=150)
                foreign = copy.deepcopy(target)
                foreign["dependencies"][1]["version"] = "foreign"
                for rejected in (None, foreign):
                    with self.assertRaises(ContractError):
                        validate_manifest(manifest, artifact, require_verified=True, tcb_evidence=evidence,
                                          runtime_fingerprint=rejected, now=150)

    def test_verified_cli_requires_independent_target_snapshot(self) -> None:
        # R5: the public CLI must load and forward the separate snapshot, not
        # derive it from observations or accept an omitted argument. The earlier
        # artifact/proof gate is mocked only to reach admission on today's
        # deliberately ineligible ledger; real JSON loading and TCB checks run.
        manifest, evidence, target = self.tcb_case()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifact = directory / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            manifest.update(schema_version=2, profile="awaken-verified-v1", profile_result={"eligible": True},
                            artifact={"name": artifact.name, "digest": digest_file(artifact)})
            for name, value in (("manifest", manifest), ("evidence", evidence), ("target", target)):
                (directory / (name + ".json")).write_bytes(canonical_bytes(value))
            arguments = ["verification_manifest.py", "validate", "--artifact", str(artifact),
                         "--manifest", str(directory / "manifest.json"), "--require-verified",
                         "--tcb-evidence", str(directory / "evidence.json")]
            with patch(f"{__name__}.build_manifest", return_value=manifest), patch("time.time", return_value=150):
                with patch.object(sys, "argv", arguments):
                    with self.assertRaisesRegex(ContractError, "target runtime fingerprint"):
                        main()
                with patch.object(sys, "argv", arguments + ["--runtime-fingerprint", str(directory / "target.json")]):
                    with patch("builtins.print"):
                        self.assertEqual(main(), 0)


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
    validate.add_argument("--runtime-fingerprint", type=Path,
                          help="independently authenticated target dependency versions/configuration digests")
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
        runtime_fingerprint=load_json(args.runtime_fingerprint) if args.runtime_fingerprint else None,
    )
    print(digest_bytes(canonical_bytes(manifest)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
