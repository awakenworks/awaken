#!/usr/bin/env python3
"""Generate and validate the canonical proof-carrying release manifest."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
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
import formal_ledger
from formal_profile import profile_result
from formal_run_evidence import canonical_bytes, committed_file_digest, digest_bytes, digest_file, proof_toolchain, validate_record

HEX64 = re.compile(r"[0-9a-f]{64}")
PROFILES = {"standard", "awaken-verified-v1"}
U64_MAX = (1 << 64) - 1
_ADMISSION_KERNEL: Path | None = None
KERNEL_DECISIONS = {
    "release": {
        "admit", "artifact-name", "artifact-digest", "profile", "proof",
        "source", "manifest",
    },
    "tcb": {
        "admit", "contract", "observed-version", "target-version", "version",
        "observed-configuration", "target-configuration", "configuration",
        "evidence-digest", "maximum-age", "observation-time", "expired",
    },
}


class ContractError(ValueError):
    pass


def _admission_kernel() -> Path:
    """Build the exact checked Rust decision source consumed by validation."""
    global _ADMISSION_KERNEL
    if _ADMISSION_KERNEL is not None:
        return _ADMISSION_KERNEL
    source_directory = (
        ROOT / "crates/server/awaken-service-lifecycle/src"
    )
    main_source = source_directory / "release_admission_main.rs"
    kernel_source = source_directory / "release_admission.rs"
    source_digest = hashlib.sha256(
        main_source.read_bytes() + b"\0" + kernel_source.read_bytes()
    ).hexdigest()
    output_directory = ROOT / "target/release-verification"
    output_directory.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if os.name == "nt" else ""
    candidate = output_directory / (
        f"awaken-verification-admission-{source_digest}{suffix}"
    )
    if candidate.is_file():
        _ADMISSION_KERNEL = candidate
        return candidate
    temporary = candidate.with_name(candidate.name + f".{os.getpid()}.tmp")
    try:
        subprocess.run(
            [
                "rustc", "--edition=2024", "-D", "warnings",
                "--cfg", 'feature="standalone-release-admission"',
                "--check-cfg", 'cfg(feature, values("standalone-release-admission"))',
                "--check-cfg", "cfg(kani)", "--check-cfg", "cfg(test)",
                "-C", "overflow-checks=yes",
                str(main_source), "-o", str(temporary),
            ],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=300,
        )
        os.replace(temporary, candidate)
    except (OSError, subprocess.SubprocessError) as error:
        temporary.unlink(missing_ok=True)
        raise ContractError(f"release admission kernel unavailable: {error}") from error
    if not candidate.is_file():
        raise ContractError("release admission kernel build produced no executable")
    _ADMISSION_KERNEL = candidate
    return candidate


def _kernel_decision(mode: str, *values: object) -> str:
    if mode not in KERNEL_DECISIONS:
        raise ContractError("unknown release admission kernel mode")
    arguments = [
        "1" if value is True else "0" if value is False else str(value)
        for value in values
    ]
    try:
        result = subprocess.run(
            [str(_admission_kernel()), mode, *arguments],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ContractError(f"release admission kernel failed closed: {error}") from error
    decision = result.stdout.strip()
    if (
        result.returncode != 0
        or "\n" in decision
        or decision not in KERNEL_DECISIONS[mode]
    ):
        raise ContractError("release admission kernel returned an invalid decision")
    return decision


def _bounded_u64(value: object, label: str) -> int:
    if type(value) is not int or not 0 <= value <= U64_MAX:
        raise ContractError(f"{label}: expected an unsigned 64-bit integer")
    return value




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
    try:
        coverage = formal_ledger.load(repository)
    except formal_ledger.LedgerError as error:
        raise ContractError(str(error)) from error
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

    def source_digest(relative: str) -> str:
        # Executed release receipts already require this exact clean Git tree.
        # Hash its blobs, not OS-dependent checkout EOLs. Unexecuted local
        # diagnostics still describe their actual working files, never a release.
        return committed_file_digest(repository, relative, source_commit) if proof_run is not None else digest_file(repository / relative)

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
        {"path": relative, "digest": source_digest(relative)}
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
        "schema_version": 3,
        "source_digest_basis": "git-blob" if proof_run is not None else "working-tree",
        "profile": profile,
        "source_commit": source_commit,
        "artifact": {
            "name": artifact.name,
            "digest": digest_file(artifact),
        },
        "ledger_digests": {
            "coverage": source_digest("formal/coverage.json"),
            "features": source_digest("formal/features.json"),
            "assumptions": source_digest("formal/assumptions.json"),
            "proof_boundaries": source_digest("formal/proof-boundaries.json"),
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
    now = _bounded_u64(now, "TCB admission clock")
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
        observed_version = item["version"]
        target_version = dependency["version"]
        observed_version_valid = (
            isinstance(observed_version, str)
            and bool(observed_version)
            and observed_version.strip() == observed_version
        )
        target_version_valid = (
            isinstance(target_version, str)
            and bool(target_version)
            and target_version.strip() == target_version
        )
        observed_configuration = item["configuration_digest"]
        target_configuration = dependency["configuration_digest"]
        maximum_age = contract.get("maximum_age_seconds")
        if type(maximum_age) is not int or maximum_age < 0:
            raise ContractError(f"{assumption_id}: invalid maximum evidence age")
        maximum_age = _bounded_u64(maximum_age, assumption_id)
        observed = item["observed_at_unix"]
        if type(observed) is not int or observed < 0:
            raise ContractError(f"{assumption_id}: invalid observation time")
        observed = _bounded_u64(observed, assumption_id)
        decision = _kernel_decision(
            "tcb",
            _sha256(contract.get("contract_digest"))
            and item["contract_digest"] == contract["contract_digest"],
            observed_version_valid,
            target_version_valid,
            observed_version == target_version,
            _sha256(observed_configuration),
            _sha256(target_configuration),
            observed_configuration == target_configuration,
            _sha256(item["evidence_digest"]),
            maximum_age,
            observed,
            now,
        )
        if decision == "admit":
            continue
        if decision == "contract":
            raise ContractError(f"{assumption_id}: contract digest mismatch")
        if decision in {"observed-version", "target-version"}:
            raise ContractError(f"{assumption_id}: invalid version")
        if decision in {"observed-configuration", "target-configuration"}:
            raise ContractError(f"{assumption_id}: invalid configuration_digest")
        if decision in {"version", "configuration"}:
            raise ContractError(f"{assumption_id}: target runtime fingerprint mismatch")
        if decision == "evidence-digest":
            raise ContractError(f"{assumption_id}: invalid evidence_digest")
        if decision == "maximum-age":
            raise ContractError(f"{assumption_id}: invalid maximum evidence age")
        if decision == "observation-time":
            raise ContractError(f"{assumption_id}: invalid observation time")
        if decision == "expired":
            raise ContractError(f"{assumption_id}: evidence expired")
        raise ContractError(f"{assumption_id}: release admission kernel failed closed")


def validate_manifest(
    manifest: dict,
    artifact: Path,
    *,
    repository: Path = ROOT,
    require_verified: bool = False,
    expected_profile: str | None = None,
    require_proof_execution: bool = False,
    tcb_evidence: dict | None = None,
    runtime_fingerprint: dict | None = None,
    now: int | None = None,
) -> None:
    if manifest.get("schema_version") != 3:
        raise ContractError("unsupported manifest schema")
    if manifest.get("profile") not in PROFILES:
        raise ContractError("unknown manifest profile")
    if expected_profile is not None and expected_profile not in PROFILES:
        raise ContractError("manifest does not match the requested release profile")
    # The release consumer already supplies the exact requested profile. Treat
    # the Verified identity itself as the admission request so omitting the
    # redundant flag cannot bypass its deployment-specific evidence boundary.
    require_verified = require_verified or expected_profile == "awaken-verified-v1"
    current_source = source_commit(repository)
    expected = build_manifest(
        artifact,
        profile=manifest["profile"],
        source_commit=current_source,
        repository=repository,
        proof_run=manifest.get("proof_execution"),
    )
    decision = _kernel_decision(
        "release",
        manifest["artifact"]["name"] == artifact.name,
        manifest["artifact"]["digest"] == digest_file(artifact),
        expected_profile is None or manifest["profile"] == expected_profile,
        not require_proof_execution or manifest.get("proof_execution") is not None,
        manifest.get("source_commit") == current_source,
        canonical_bytes(manifest) == canonical_bytes(expected),
    )
    release_errors = {
        "artifact-name": "artifact name mismatch",
        "artifact-digest": "artifact digest mismatch",
        "profile": "manifest does not match the requested release profile",
        "proof": "release consumption requires completed strict formal execution evidence",
        "source": "manifest source does not match the exact release source",
        "manifest": "manifest does not match the exact source, proof inputs, or ledgers",
    }
    if decision != "admit":
        raise ContractError(release_errors.get(
            decision, "release admission kernel returned an unknown disposition"
        ))
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
    def release_case(directory: Path) -> tuple[Path, Path, dict]:
        # Reuse the receipt fixture and production archive builder. The bytes
        # are deliberately synthetic; no real binary or proof tool is claimed.
        from test_formal_run_evidence import prepare_source_fixture
        from formal_run_evidence import begin_record, finish_record
        from package import create_archive, write_checksum
        row = dict(id="fixture.safe", formalizable=True, claim_scope="repository_safety",
                   proof_grade="abstract_model_checked", bounds=["fixture only"], evidence=["formal/tla/Fixture.tla"])
        feature = dict(id="fixture", criticality="P0", requirements=[dict(
            kind="safety", repository_controlled=True, obligations=["fixture.safe"])])
        root = directory / "source"
        prepare_source_fixture(root, {
            ".gitignore": "target/\ndist/\ndeploy/images/management/.awaken-management.bin\ndeploy/images/management/.awaken-verification.json\n",
            "formal/coverage.json": json.dumps({"obligations": [row]}) + "\n",
            "formal/features.json": json.dumps({"features": [feature]}) + "\n",
            "formal/assumptions.json": '{"assumptions": []}\n',
            "formal/proof-boundaries.json": '{"boundaries": []}\n',
            "formal/tla/Fixture.tla": "---- MODULE Fixture ----\n====\n",
            "scripts/ci/formal_profile.py": (ROOT / "scripts/ci/formal_profile.py").read_text(),
            "scripts/release/verification_manifest.py": Path(__file__).read_text(),
            "crates/server/awaken-service-lifecycle/src/release_admission.rs": (
                ROOT / "crates/server/awaken-service-lifecycle/src/release_admission.rs"
            ).read_text(),
            "crates/server/awaken-service-lifecycle/src/release_admission_main.rs": (
                ROOT / "crates/server/awaken-service-lifecycle/src/release_admission_main.rs"
            ).read_text(),
            "README.md": "fixture\n", "LICENSE": "fixture\n", "deploy/compose.yaml": "# fixture\n",
        })
        (root / "deploy/images/management").mkdir(parents=True)
        binary = directory / "binary"
        binary.write_bytes(b"synthetic binary")
        artifact = create_archive(root, binary, binary, "x86_64-unknown-linux-gnu", "1.0.0", root / "dist", 100)
        write_checksum(artifact)
        receipt = finish_record(begin_record(root, True, 100), root, False, 110)
        manifest = build_manifest(artifact, profile="standard", source_commit=source_commit(root),
                                  repository=root, proof_run=receipt)
        write_manifest(manifest, artifact.with_name(artifact.name + ".verification.json"))
        return root, artifact, manifest

    def test_release_source_hashes_are_git_normalized_but_real_changes_reject(self) -> None:
        # E1 identical clean Git source + artifact + synthetic receipt across
        # LF/CRLF checkouts => identical manifests. E2 a semantic source edit
        # after conversion rejects, never normalized away. This is a packaging
        # fixture, not evidence of any actual formal tool execution.
        with tempfile.TemporaryDirectory() as temporary:
            root, artifact, before = self.release_case(Path(temporary))
            windows = Path(temporary) / "windows"
            subprocess.run(["git", "clone", "--quiet", "--config", "core.autocrlf=true", str(root), str(windows)],
                           check=True, capture_output=True)
            model = windows / "formal/tla/Fixture.tla"
            self.assertIn(b"\r\n", model.read_bytes())
            after = build_manifest(artifact, profile="standard", source_commit=source_commit(windows),
                                   repository=windows, proof_run=before["proof_execution"])
            self.assertEqual(before, after)
            validate_manifest(before, artifact, repository=windows)
            model.write_bytes(model.read_bytes() + b"changed\r\n")
            with self.assertRaisesRegex(ContractError, "clean release source"):
                validate_manifest(before, artifact, repository=windows)

    def test_kernel_process_boundary_has_no_admission_fallback(self) -> None:
        # Cause/effect table: C1 exact compiled source + valid release facts ->
        # R1 Admit. C2 missing executable, C3 nonzero process result, C4 unknown
        # or multiline output, and C5 unknown mode -> R2-R5 ContractError. Every
        # process-boundary fault rejects; Python never substitutes its own allow.
        self.assertEqual(_kernel_decision("release", *([True] * 6)), "admit", "R1")
        with self.subTest(rule="R2"), patch(
            f"{__name__}._admission_kernel",
            return_value=Path("/missing/awaken-verification-admission"),
        ):
            with self.assertRaises(ContractError):
                _kernel_decision("release", *([True] * 6))
        for rule, result in (
            ("R3", subprocess.CompletedProcess(["kernel"], 2, "", "failed")),
            ("R4.unknown", subprocess.CompletedProcess(["kernel"], 0, "unknown\n", "")),
            ("R4.multiline", subprocess.CompletedProcess(["kernel"], 0, "admit\nextra\n", "")),
        ):
            with self.subTest(rule=rule), patch(
                f"{__name__}._admission_kernel", return_value=Path("kernel")
            ), patch(f"{__name__}.subprocess.run", return_value=result):
                with self.assertRaises(ContractError):
                    _kernel_decision("release", *([True] * 6))
        with self.assertRaises(ContractError):
            _kernel_decision("unknown", True)

    def test_release_consumption_requires_exact_profile_and_executed_proofs(self) -> None:
        # R1 standard release with a valid receipt accepts without claiming a
        # Verified runtime. R2 local diagnostic (null receipt), R3 other profile,
        # R4 running/foreign receipt and R5 schema/digest/bounds tampering reject.
        # Use real rebuild/receipt validation; no qualification mocks here.
        with tempfile.TemporaryDirectory() as temporary:
            root, artifact, manifest = self.release_case(Path(temporary))
            validate_manifest(manifest, artifact, repository=root, expected_profile="standard", require_proof_execution=True)
            diagnostic = build_manifest(artifact, profile="standard", source_commit=source_commit(root), repository=root)
            validate_manifest(diagnostic, artifact, repository=root)
            with self.assertRaisesRegex(ContractError, "execution evidence"):
                validate_manifest(diagnostic, artifact, repository=root, require_proof_execution=True)
            for profile in ("awaken-verified-v1", "unknown"):
                with self.subTest(profile=profile), self.assertRaisesRegex(ContractError, "requested release profile"):
                    validate_manifest(manifest, artifact, repository=root, expected_profile=profile, require_proof_execution=True)
            mutations = [dict(manifest, schema_version=2), dict(manifest, source_digest_basis="working-tree"),
                         dict(manifest, bounds=["invented"]), dict(manifest, proof_inputs_digest="sha256:" + "0" * 64),
                         dict(manifest, proof_execution=dict(manifest["proof_execution"], status="running")),
                         dict(manifest, proof_execution=dict(manifest["proof_execution"], runner={}))]
            for index, changed in enumerate(mutations):
                with self.subTest(rule=index), self.assertRaises(ContractError):
                    validate_manifest(changed, artifact, repository=root, expected_profile="standard", require_proof_execution=True)

    def test_actual_release_consumers_reject_before_extract_or_attest(self) -> None:
        # C1 actual workflow shell, C2 matching profile/artifact/receipt, C3
        # intact sidecar. R1 all true -> stage/validation succeeds; R2 missing
        # or null/running evidence, R3 wrong profile, R4 changed archive,
        # R5 orphan sidecar -> failure before extraction or final attestation.
        # Execute only local validation/staging blocks, never registry writes.
        from check_product_release import shell_step
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        for step in ("Stage the exact packaged binary", "Validate every packaged proof manifest"):
            script = shell_step(workflow, step)
            for fault in (None, "missing", "null", "running", "profile", "archive", "orphan"):
                if step.startswith("Stage") and fault == "orphan":
                    continue  # Final publication owns the entire sidecar set.
                with self.subTest(step=step, fault=fault), tempfile.TemporaryDirectory() as temporary:
                    root, artifact, manifest = self.release_case(Path(temporary))
                    sidecar = artifact.with_name(artifact.name + ".verification.json")
                    if fault == "missing":
                        sidecar.unlink()
                    elif fault in {"null", "running"}:
                        manifest["proof_execution"] = None if fault == "null" else dict(manifest["proof_execution"], status="running")
                        write_manifest(manifest, sidecar)
                    elif fault == "archive":
                        artifact.write_bytes(b"changed archive")
                    elif fault == "orphan":
                        write_manifest(manifest, artifact.parent / "awaken-v-orphan.verification.json")
                    result = subprocess.run(["bash", "-c", script], cwd=root, capture_output=True, text=True, timeout=30,
                                            env=dict(os.environ, AWAKEN_RELEASE_PROFILE="awaken-verified-v1" if fault == "profile" else "standard",
                                                     PYTHONDONTWRITEBYTECODE="1"))
                    staged = root / "deploy/images/management/.awaken-management.bin"
                    if fault is None:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        if step.startswith("Stage"):
                            self.assertEqual(staged.read_bytes(), b"synthetic binary")
                            self.assertEqual((staged.parent / ".awaken-verification.json").read_bytes(), sidecar.read_bytes())
                    else:
                        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertFalse(staged.exists())
                        self.assertFalse((staged.parent / ".awaken-verification.json").exists())

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
        # Cause/effect table: C1 explicit require_verified OR C2 the release
        # consumer's expected profile is Verified requests deployment admission;
        # C3 fresh TCB evidence and C4 an independently supplied exact target
        # admit. R1 C1+C3+C4 and R2 C2+C3+C4 accept through the same validator;
        # R3 either request with missing evidence, missing target, or a foreign
        # target rejects. The earlier proof/ledger gate alone is mocked so this
        # isolates admission wiring.
        manifest, evidence, target = self.tcb_case()
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            manifest.update(schema_version=3, profile="awaken-verified-v1", profile_result={"eligible": True},
                            source_commit=source_commit(ROOT),
                            artifact={"name": artifact.name, "digest": digest_file(artifact)})
            with patch(f"{__name__}.build_manifest", return_value=manifest):
                validate_manifest(manifest, artifact, require_verified=True, tcb_evidence=evidence,
                                  runtime_fingerprint=target, now=150)
                validate_manifest(manifest, artifact, expected_profile="awaken-verified-v1",
                                  tcb_evidence=evidence, runtime_fingerprint=target, now=150)
                foreign = copy.deepcopy(target)
                foreign["dependencies"][1]["version"] = "foreign"
                for rejected_evidence, rejected_target, message in (
                    (None, target, "TCB evidence"),
                    (evidence, None, "target runtime fingerprint"),
                    (evidence, foreign, "fingerprint mismatch"),
                ):
                    with self.subTest(message=message), self.assertRaisesRegex(
                        ContractError, message
                    ):
                        validate_manifest(
                            manifest, artifact,
                            expected_profile="awaken-verified-v1",
                            tcb_evidence=rejected_evidence,
                            runtime_fingerprint=rejected_target,
                            now=150,
                        )

    def test_verified_cli_requires_independent_target_snapshot(self) -> None:
        # Cause/effect table: C1 expected-profile=Verified; C2 evidence path;
        # C3 independent target path. R1 C1+C2+C3 admits; R2 C1 without C2/C3
        # rejects missing evidence; R3 C1+C2 without C3 rejects missing target.
        # No explicit --require-verified flag is present: the public CLI must
        # derive the stronger gate from the identity requested by release, then
        # load and forward both existing file inputs. The earlier proof/ledger
        # gate is mocked only to reach this real TCB decision.
        manifest, evidence, target = self.tcb_case()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifact = directory / "awaken.tar.gz"
            artifact.write_bytes(b"artifact")
            manifest.update(schema_version=3, profile="awaken-verified-v1", profile_result={"eligible": True},
                            source_commit=source_commit(ROOT),
                            artifact={"name": artifact.name, "digest": digest_file(artifact)})
            for name, value in (("manifest", manifest), ("evidence", evidence), ("target", target)):
                (directory / (name + ".json")).write_bytes(canonical_bytes(value))
            arguments = ["verification_manifest.py", "validate", "--artifact", str(artifact),
                         "--manifest", str(directory / "manifest.json"),
                         "--expected-profile", "awaken-verified-v1"]
            with patch(f"{__name__}.build_manifest", return_value=manifest), patch("time.time", return_value=150):
                with patch.object(sys, "argv", arguments):
                    with self.assertRaisesRegex(ContractError, "TCB evidence"):
                        main()
                with patch.object(sys, "argv", arguments + [
                    "--tcb-evidence", str(directory / "evidence.json")
                ]):
                    with self.assertRaisesRegex(ContractError, "target runtime fingerprint"):
                        main()
                with patch.object(sys, "argv", arguments + [
                    "--tcb-evidence", str(directory / "evidence.json"),
                    "--runtime-fingerprint", str(directory / "target.json"),
                ]):
                    with patch("builtins.print"):
                        self.assertEqual(main(), 0)


def source_commit(repository: Path) -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True
    ).strip()


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
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
    validate.add_argument("--expected-profile", choices=sorted(PROFILES))
    validate.add_argument("--require-proof-execution", action="store_true")
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
        expected_profile=args.expected_profile,
        require_proof_execution=args.require_proof_execution,
        tcb_evidence=evidence,
        runtime_fingerprint=load_json(args.runtime_fingerprint) if args.runtime_fingerprint else None,
    )
    print(digest_bytes(canonical_bytes(manifest)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
