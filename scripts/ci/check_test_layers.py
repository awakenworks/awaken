#!/usr/bin/env python3
"""Check source/design links only; never compile or certify test execution."""
from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import re
import sys
import unittest

INPUT_GLOBS = (
    "docs/design/test-layer-matrix.json", "docs/windows-*-20261023.json",
    "crates/server/awaken-run-ingress/**", "crates/server/awaken-worker-registry/**",
    "e2e/**", "scripts/ci/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
    ".cargo/**", ".config/**", ".github/workflows/**", "lefthook.yml",
)
ROOT = Path(__file__).resolve().parents[2]
MATRIX = "docs/design/test-layer-matrix.json"
REQUIRED_ROWS = frozenset((
    "worker-registry", "lease-epoch", "claim-races", "pending-idempotency",
    "checkpoint-corruption", "scheduling-recovery", "ingress-parity", "pg-notify",
    "nats-delivery", "dispatch-scale", "scale-reporting", "served-postgres",
    "native-sql-observation", "windows-host-admission", "cold-and-full-ci",
))


def required_layers(paths: list[str], *, qualification: bool = False) -> dict:
    """Conservative obligations; existing Cargo owner/consumer planner owns L1.

    This never executes or substitutes for a gate. Unknown inputs widen the
    obligations, so an affected-Rust pass cannot silently become service or CI
    qualification. Cold and scale stay separate requirements inside L4.
    """
    layers = {"L0"}; reasons = []; cold = False; scale = False
    for raw in paths:
        path = raw.replace("\\", "/")
        if path.startswith("/") or ".." in path.split("/") or re.match(r"^[A-Za-z]:", path):
            raise ValueError("changed paths must be repository-relative")
        if path.startswith(("docs/", "wiki/")) or path.endswith(".md"):
            continue
        if (path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "lefthook.yml")
                or path.endswith("/Cargo.toml") or path.startswith((".cargo/", ".config/", ".github/", "scripts/ci/"))):
            layers.update(("L1", "L2", "L3", "L4")); cold = True
            reasons.append(path + ": global build/gate input; keep whole Rust graph and cold qualification")
        elif path.startswith(("crates/", "apps/", "tests/", "xtask/", "fuzz/")):
            layers.update(("L1", "L2", "L3"))
            reasons.append(path + ": affected owners plus transitive consumers; service and process laws remain separate")
            if ("awaken-run-ingress/" in path and ("/migrations/" in path or "/src/postgres" in path
                    or path.endswith("/dispatch_claim_perf.rs"))):
                layers.add("L4"); scale = True
        elif path.startswith(("e2e/", "web/", "packages/", "contracts/", "deploy/")):
            layers.update(("L2", "L3"))
            reasons.append(path + ": actual service/process or consumer assertions cannot be replaced by pure runner tests")
        else:
            layers.update(("L1", "L2", "L3", "L4")); cold = True; scale = True
            reasons.append(path + ": unknown ownership; refuse narrow qualification")
    if qualification:
        layers.update(("L1", "L2", "L3", "L4")); cold = True; scale = True
        reasons.append("explicit complete qualification; changed-file selection cannot omit any layer")
    return dict(required_layers=sorted(layers), cold_required=cold, scale_required=scale,
                full_gate=False, execution_performed=False, reasons=reasons,
                rust_selection="scripts/ci/rust_test_plan.py: owner + transitive consumer closure; global/unowned/manifest inputs keep whole graph",
                acceptance="Each required layer needs its own same-source execution evidence. A plan or a quick Rust pass satisfies none of the remaining layers.")


def validate(value: dict, root: Path = ROOT) -> None:
    if (value.get("schema_version") != 1 or value.get("status") != "source-reviewed-design"
            or value.get("full_gate") is not False
            or value.get("execution_performed") is not False):
        raise ValueError("a source matrix cannot certify a native execution or a full gate")
    if set(value.get("tiers", {})) != {"L0", "L1", "L2", "L3", "L4"}:
        raise ValueError("missing test tier")
    rows = value.get("rows", [])
    ids = [row.get("id") for row in rows]
    if len(ids) != len(set(ids)) or set(ids) != REQUIRED_ROWS:
        raise ValueError("missing or duplicate risk domain")
    identities = set()
    for row in rows:
        if (row.get("tier") not in value["tiers"] or row.get("priority") not in ("P0", "P1")
                or row.get("execution") != "deferred"):
            raise ValueError("source-only rows must keep explicit unexecuted status")
        for key in ("conditions", "assertions", "remaining", "cases"):
            if not isinstance(row.get(key), list) or not row[key]:
                raise ValueError("risk domain is missing conditions, assertions, cases or remaining work")
        entry = row.get("entry", {})
        if not entry.get("state") or not isinstance(entry.get("argv"), list) or not entry["argv"]:
            raise ValueError("selection must be an explicit command and implementation state")
        for case in row["cases"]:
            relative = Path(case["source"])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("case source must stay in the repository")
            path = root / relative
            identity = (case["package"], case["target"], case["name"])
            if identity in identities:
                raise ValueError("duplicate case reference; do not count it twice")
            identities.add(identity)
            text = path.read_text(encoding="utf-8")
            name = case["name"].split("::")[-1]
            pattern = (r"\bfn\s+" + re.escape(name) + r"\s*\(" if path.suffix == ".rs"
                       else r"\bdef\s+" + re.escape(name) + r"\s*\(" if path.suffix == ".py"
                       else re.escape(case["name"]))
            if not re.search(pattern, text):
                raise ValueError("missing source anchor: " + case["source"] + ":" + case["name"])
    by_id = {row["id"]: row for row in rows}
    broker = by_id["nats-delivery"]["entry"]["argv"]
    if "--features" not in broker or not {"test-support", "nats"} <= set(broker[broker.index("--features") + 1].split(",")):
        raise ValueError("NATS is not implied by test-support")
    scale = by_id["dispatch-scale"]["entry"]["argv"]
    if ("--ignored" not in scale or "--nocapture" in scale or "--show-output" not in scale
            or len(by_id["dispatch-scale"]["cases"]) != 3):
        raise ValueError("scale must execute exactly three ignored observations with captured output")
    # Inspect the source inventory, not compiler inventory. In particular, a
    # fourth live lifecycle case must not disappear behind the old three-case plan.
    notify = root / "crates/server/awaken-run-ingress/tests/pg_notify_wake.rs"
    notify_names = set(re.findall(r"#\[tokio::test[^\]]*\]\s*async fn (\w+)\(", notify.read_text(encoding="utf-8")))
    if {case["name"] for case in by_id["pg-notify"]["cases"]} != notify_names:
        raise ValueError("PgNotify source test inventory drifted")
    if len(by_id["windows-host-admission"]["cases"]) != 6:
        raise ValueError("Windows host admission must retain all six explicit gaps")
    lease = by_id["lease-epoch"]
    if {rule.get("id") for rule in lease.get("decision_rules", [])} != {"LR-current", "LR-unreclaimed-expired", "LR-reused-owner", "LR-guard", "LR-physical"}:
        raise ValueError("missing lease boundary decision")
    for rule in lease.get("decision_rules", []):
        references = {rule.get("existing_case"), *rule.get("companion_cases", [])}
        if not references <= {case["name"] for case in lease["cases"]}:
            raise ValueError("lease decision rule cites an absent case")
    if {rule.get("id") for rule in lease.get("planned_regressions", [])} != {"renew-before-reclaim", "reclaim-before-renew"}:
        raise ValueError("expiry race needs both forced transaction orders")
    admission = value.get("shared_admission_design", {})
    if (admission.get("implementation") != "deferred-rust" or len(admission.get("targets", [])) != 6
            or {(rule.get("required"), rule.get("url")) for rule in admission.get("rules", [])}
            != {(required, url) for required in (False, True) for url in ("absent", "empty", "unreachable", "owned-live")}):
        raise ValueError("missing required/optional actual-child fixture decision")
    for profile in value.get("closed_profiles", []):
        receipt = Path(profile.get("receipt", ""))
        digest = profile.get("receipt_sha256", "")
        if (not receipt.parts or receipt.is_absolute() or ".." in receipt.parts
                or not re.fullmatch(r"[a-f0-9]{64}", digest)):
            raise ValueError("historical scope needs a relative receipt and original digest")
        # This is a source/design reference. Artifact absence is not an
        # execution failure or evidence of a successful run.
    if {row.get("id") for row in value.get("trigger_rules", [])} != {"always", "rust", "service-process", "global", "scale", "unknown", "qualification"}:
        raise ValueError("missing layer trigger rule")


class MatrixTests(unittest.TestCase):
    def fixture(self):
        return json.loads((ROOT / MATRIX).read_text(encoding="utf-8"))

    def test_source_matrix_does_not_select_a_validation_host(self):
        # C1 applicable host label varies; C2 source links and design rows are
        # unchanged. Every C1 value keeps E1 the same source-design admission;
        # actual host execution and publication authority remain external.
        for host in ("Linux", "Windows/MSVC", "macOS"):
            value = self.fixture()
            value["platform_scope"] = host
            with self.subTest(host=host):
                validate(value)

    def test_archived_receipts_keep_identity_without_claiming_execution(self):
        # C1 original digest valid/invalid; C2 locator relative/traversing;
        # C3 bytes locally absent. R1 valid+relative+absent -> design accepted,
        # not execution certified; R2 bad digest or traversal -> refuse.
        value = self.fixture()
        value["closed_profiles"] = [{"receipt": "target/unavailable/receipt.json",
                                     "receipt_sha256": "a" * 64}]
        validate(value)
        for field, bad in (("receipt", "../outside.json"),
                           ("receipt_sha256", "unknown")):
            refused = copy.deepcopy(value)
            refused["closed_profiles"][0][field] = bad
            with self.subTest(field=field), self.assertRaises(ValueError):
                validate(refused)

    def test_current_design_links_to_existing_source_without_certifying_execution(self):
        validate(self.fixture())

    def test_missing_domain_duplicate_case_or_deleted_source_cannot_pass(self):
        for fault in ("domain", "duplicate-domain", "duplicate-case", "source", "symbol", "condition", "path"):
            value = self.fixture()
            if fault == "domain": value["rows"].pop()
            elif fault == "duplicate-domain": value["rows"].append(copy.deepcopy(value["rows"][0]))
            elif fault == "duplicate-case": value["rows"][0]["cases"].append(copy.deepcopy(value["rows"][0]["cases"][0]))
            elif fault == "source": value["rows"][0]["cases"][0]["source"] = "missing.rs"
            elif fault == "symbol": value["rows"][0]["cases"][0]["name"] = "not_a_test_case"
            elif fault == "condition": value["rows"][0]["conditions"] = []
            else: value["rows"][0]["cases"][0]["source"] = "../outside.rs"
            with self.subTest(fault=fault), self.assertRaises((ValueError, FileNotFoundError)):
                validate(value)

    def test_feature_scale_inventory_and_execution_claim_mutants_fail(self):
        for fault in ("nats", "ignored", "capture", "lifecycle", "pass", "execution", "full-gate"):
            value = self.fixture(); rows = {row["id"]: row for row in value["rows"]}
            if fault == "nats":
                args = rows["nats-delivery"]["entry"]["argv"]; args[args.index("--features") + 1] = "test-support"
            elif fault == "ignored": rows["dispatch-scale"]["entry"]["argv"].remove("--ignored")
            elif fault == "capture": rows["dispatch-scale"]["entry"]["argv"].append("--nocapture")
            elif fault == "lifecycle": rows["pg-notify"]["cases"].pop()
            elif fault == "pass": value["rows"][0]["execution"] = "passed"
            elif fault == "execution": value["execution_performed"] = True
            else: value["full_gate"] = True
            with self.subTest(fault=fault), self.assertRaises(ValueError): validate(value)

    def test_missing_race_order_or_admission_condition_and_wrong_law_reference_fail(self):
        for fault in ("race", "law", "missing-law", "admission", "target"):
            value = self.fixture(); lease = next(row for row in value["rows"] if row["id"] == "lease-epoch")
            if fault == "race": lease["planned_regressions"].pop()
            elif fault == "law": lease["decision_rules"][0]["existing_case"] = "absent"
            elif fault == "missing-law": lease["decision_rules"].pop()
            elif fault == "admission": value["shared_admission_design"]["rules"].pop()
            else: value["shared_admission_design"]["targets"].pop()
            with self.subTest(fault=fault), self.assertRaises(ValueError): validate(value)

    def test_quick_selection_retains_service_process_cold_and_scale_obligations(self):
        # Regression causes: a backend .rs change, its Cargo feature definition,
        # a Node-only harness change, an unknown new input and an explicit full
        # qualification. None can be certified by affected Rust alone.
        for paths,layers,cold,scale in [
            (["docs/design/test-validation.md"], {"L0"}, False, False),
            (["crates/server/awaken-run-ingress/src/lease.rs"], {"L0","L1","L2","L3"}, False, False),
            (["crates/server/awaken-run-ingress/Cargo.toml"], {"L0","L1","L2","L3","L4"}, True, False),
            (["e2e/harness.mjs"], {"L0","L2","L3"}, False, False),
            (["crates/server/awaken-run-ingress/src/postgres_claim.rs"], {"L0","L1","L2","L3","L4"}, False, True),
            (["new-build-input"], {"L0","L1","L2","L3","L4"}, True, True),
        ]:
            with self.subTest(paths=paths):
                result=required_layers(paths)
                self.assertEqual(set(result["required_layers"]),layers)
                self.assertEqual((result["cold_required"],result["scale_required"]),(cold,scale))
                self.assertIs(result["full_gate"],False);self.assertIs(result["execution_performed"],False)
        self.assertEqual(required_layers([],qualification=True)["required_layers"],["L0","L1","L2","L3","L4"])
        self.assertEqual(required_layers(["docs/a.md","Cargo.lock"])["required_layers"],["L0","L1","L2","L3","L4"])
        for path in ("../outside", "/absolute", "C:/absolute"):
            with self.assertRaises(ValueError):required_layers([path])


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--plan-base", help="derive obligations from the canonical index/working/untracked change reader")
    parser.add_argument("--qualification", action="store_true", help="require every layer, including independent scale and cold validation")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(MatrixTests)).wasSuccessful() else 1
    if args.plan_base:
        from rust_test_plan import changed_sources, working_subject
        before,_=working_subject();paths=changed_sources(args.plan_base)
        plan=required_layers(paths,qualification=args.qualification)
        if working_subject()[0]!=before:raise ValueError("source changed during layer planning")
        print(json.dumps(dict(plan,source_digest=before,changed_paths=paths),indent=2));return 0
    if args.qualification:parser.error("--qualification requires --plan-base")
    value = json.loads((ROOT / MATRIX).read_text(encoding="utf-8")); validate(value)
    print(f"Source-design test layer matrix: {len(value['rows'])} risk domains; source links valid; execution remains deferred")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
