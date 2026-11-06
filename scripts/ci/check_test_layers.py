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

from check_test_inventory import test_names as rust_test_names
from _crate_boundary_workspace import rust_without_comments

# Layer selection considers every changed path, including newly relocated owners.
# The pure policy checks remain the always-required L0 lane.
INPUT_GLOBS = ("**/*",)

ROOT = Path(__file__).resolve().parents[2]
MATRIX = "docs/design/test-layer-matrix.json"
REQUIRED_ROWS = frozenset((
    "worker-registry", "lease-epoch", "claim-races", "pending-idempotency",
    "checkpoint-corruption", "scheduling-recovery", "ingress-parity", "pg-notify",
    "nats-delivery", "dispatch-scale", "scale-reporting", "served-postgres",
    "native-sql-observation", "windows-host-admission", "cold-and-full-ci",
))



def required_layers(paths: list[str], *, qualification: bool = False,
                    metadata: dict | None = None, root: Path = ROOT) -> dict:
    """Derive tier obligations from the SAME owners/consumer closure as L1.

    Missing ownership information widens Rust inputs to complete qualification.
    Package names identify the benchmark owner; directory layout is not a policy.
    A crate README can be Rust input, including rustdoc's include_str! content.
    """
    from rust_test_plan import gate_inputs, gate_scope
    from generate_hooks import glob_matches
    from crate_test import _members
    layers = {"L0"}; reasons = []; cold = False; scale = False; rust_inputs = []
    if metadata is not None:
        names = {package["name"] for package in _members(metadata).values()}
        if "awaken-dispatch" not in names:
            raise ValueError("complete Cargo graph must contain the scale benchmark owner")
    for raw in paths:
        path = raw.replace("\\", "/")
        if path.startswith("/") or ".." in path.split("/") or re.match(r"^[A-Za-z]:", path):
            raise ValueError("changed paths must be repository-relative")
        if ((path.startswith(("docs/", "wiki/")) and path.endswith(".md"))
                or path == "README.md"):
            continue
        if (path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "lefthook.yml")
                or path.endswith("/Cargo.toml") or path.startswith((".cargo/", ".config/", ".github/", "scripts/ci/"))):
            layers.update(("L1", "L2", "L3", "L4")); cold = True
            rust_inputs.append(path)
            reasons.append(path + ": global build/gate input; keep whole Rust graph and cold qualification")
            continue
        owned = False
        if metadata is not None:
            kind, _, _ = gate_scope(metadata, [path], root, inputs=("**/*",))
            owned = kind == "packages"
        if owned:
            layers.update(("L1", "L2", "L3")); rust_inputs.append(path)
            reasons.append(path + ": canonical Cargo owners and transitive consumers, including test dependencies")
        elif path.startswith(("e2e/", "web/", "packages/", "contracts/", "deploy/")):
            if any(glob_matches(pattern, path) for pattern in gate_inputs()):
                layers.update(("L1", "L2", "L3", "L4")); cold = True; scale = True
                rust_inputs.append(path)
                reasons.append(path + ": declared external Rust input has no Cargo member owner; refuse narrow service-only qualification")
            else:
                layers.update(("L2", "L3"))
                reasons.append(path + ": actual service/process or consumer assertions remain separate")
        else:
            layers.update(("L1", "L2", "L3", "L4")); cold = True; scale = True
            reasons.append(path + ": unknown ownership; refuse narrow qualification")
    if rust_inputs:
        if metadata is None:
            layers.add("L4"); cold = True; scale = True
            reasons.append("Cargo ownership unavailable; retain complete cold and scale obligations")
        else:
            _, consumers, _ = gate_scope(metadata, rust_inputs, root, inputs=("**/*",))
            if "awaken-dispatch" in consumers:
                layers.add("L4"); scale = True
                reasons.append("actual Cargo consumer closure reaches the scale benchmark owner")
    if qualification:
        layers.update(("L1", "L2", "L3", "L4")); cold = True; scale = True
        reasons.append("explicit complete qualification; changed-file selection cannot omit any layer")
    return dict(required_layers=sorted(layers), cold_required=cold, scale_required=scale,
                full_gate=False, execution_performed=False, reasons=reasons,
                rust_selection="scripts/ci/rust_test_plan.py: owner + transitive consumer closure; global/unowned/manifest inputs keep whole graph",
                acceptance="Each required layer needs its own same-source execution evidence. A plan or a quick Rust pass satisfies none of the remaining layers.")



def validate_scale_inventory(value: dict, source: str) -> None:
    """The ignored and ordinary rows together own every authored scale test."""
    rows = {row["id"]: row for row in value["rows"]}
    declared = {case["name"] for name in ("dispatch-scale", "scale-reporting")
                for case in rows[name]["cases"]}
    actual = set(rust_test_names(source))
    if not actual or declared != actual:
        raise ValueError("scale/reporting source inventory differs: missing="
                         + repr(sorted(actual - declared)) + ", stale="
                         + repr(sorted(declared - actual)))

    code = rust_without_comments(source)
    attributes = {
        match.group(2): match.group(1)
        for match in re.finditer(r"((?:#\[[^\]]*\]\s*)+)(?:async\s+)?fn\s+(\w+)\s*\(", code)
        if match.group(2) in actual
    }
    ignored = {name for name, attrs in attributes.items() if re.search(r"#\[\s*ignore\b", attrs)}
    if {case["name"] for case in rows["dispatch-scale"]["cases"]} != ignored:
        raise ValueError("scale ignored classification differs from authored attributes")
    if {case["name"] for case in rows["scale-reporting"]["cases"]} != actual - ignored:
        raise ValueError("ordinary reporting classification differs from authored attributes")
    scale = rows["dispatch-scale"]
    authored_variables = set(re.findall(r'"(AWAKEN_DISPATCH_PERF_[A-Z_]+)"', code))
    if set(scale.get("clear_environment", [])) != authored_variables:
        raise ValueError("scale must clear every authored workload override")
    base = ["cargo", "test", "--locked", "-p", "awaken-dispatch", "--features",
            "test-support", "--test", "dispatch_claim_perf"]
    if scale["entry"]["argv"] != [*base, "--", "--ignored", "--test-threads=1", "--show-output"]:
        raise ValueError("base scale selection must retain all ignored cases")
    concurrent = "concurrent_incompatible_front_claim_throughput"
    parameter_runs = [{
        "environment": {"AWAKEN_DISPATCH_PERF_PREFIX_KIND": "capability"},
        "argv": [*base, "--", concurrent, "--exact", "--ignored", "--test-threads=1", "--show-output"],
    }]
    if scale.get("parameter_runs") != parameter_runs:
        raise ValueError("scale requires the separate exact capability concurrent branch at default workload")


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
    if "--ignored" not in scale or "--nocapture" in scale or "--show-output" not in scale:
        raise ValueError("scale must execute ignored observations with captured output")
    validate_scale_inventory(value, (root / "crates/platform/dispatch/awaken-dispatch/tests/dispatch_claim_perf.rs")
                             .read_text(encoding="utf-8"))
    # Inspect the source inventory, not compiler inventory. In particular, a
    # fourth live lifecycle case must not disappear behind the old three-case plan.
    notify = root / "crates/platform/dispatch/awaken-dispatch/tests/pg_notify_wake.rs"
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
    if {row.get("id") for row in value.get("trigger_rules", [])} != {"always", "rust", "external-rust-input", "service-process", "global", "scale", "unknown", "qualification"}:
        raise ValueError("missing layer trigger rule")


class MatrixTests(unittest.TestCase):
    def fixture(self):
        return json.loads((ROOT / MATRIX).read_text(encoding="utf-8"))

    def test_current_design_links_to_existing_source_without_certifying_execution(self):
        validate(self.fixture())

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

    def test_every_layer_trigger_rule_is_required(self):
        original = self.fixture()
        for rule in original["trigger_rules"]:
            with self.subTest(rule=rule["id"]):
                value = copy.deepcopy(original)
                value["trigger_rules"] = [item for item in value["trigger_rules"]
                                          if item["id"] != rule["id"]]
                with self.assertRaisesRegex(ValueError, "missing layer trigger rule"):
                    validate(value)

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

    def test_omitting_the_capability_prefix_measurement_cannot_pass(self):
        # An existing count of three was green while the fourth authored
        # ignored measurement was absent from every matrix obligation.
        value = self.fixture()
        row = next(row for row in value["rows"] if row["id"] == "dispatch-scale")
        row["cases"] = [case for case in row["cases"]
                        if case["name"] != "capability_incompatible_front_queue_claim_latency"]
        with self.assertRaisesRegex(ValueError, "scale/reporting source inventory differs"):
            validate(value)

    def test_a_new_unregistered_source_measurement_cannot_pass(self):
        value = self.fixture()
        source = (ROOT / "crates/platform/dispatch/awaken-dispatch/tests/dispatch_claim_perf.rs").read_text(encoding="utf-8")
        source += '\n#[tokio::test]\n#[ignore = "explicit native scale fixture"]\nasync fn new_unregistered_measurement() {}\n'
        with self.assertRaisesRegex(ValueError, "new_unregistered_measurement"):
            validate_scale_inventory(value, source)


    def test_ignored_classification_and_parameter_branch_cannot_disappear(self):
        for fault in ("moved-to-reporting", "missing-parameter", "wrong-prefix",
                      "reduced-workload", "missing-clear", "filtered-base"):
            value = self.fixture()
            rows = {row["id"]: row for row in value["rows"]}
            scale = rows["dispatch-scale"]
            if fault == "moved-to-reporting":
                rows["scale-reporting"]["cases"].append(scale["cases"].pop())
            elif fault == "missing-parameter":
                scale.pop("parameter_runs")
            elif fault == "wrong-prefix":
                scale["parameter_runs"][0]["environment"]["AWAKEN_DISPATCH_PERF_PREFIX_KIND"] = "exhausted_epoch"
            elif fault == "reduced-workload":
                scale["parameter_runs"][0]["environment"]["AWAKEN_DISPATCH_PERF_ROWS"] = "1"
            elif fault == "missing-clear":
                scale["clear_environment"].remove("AWAKEN_DISPATCH_PERF_PREFIX_KIND")
            else:
                scale["entry"]["argv"].append("capability_incompatible_front_queue_claim_latency")
            with self.subTest(fault=fault), self.assertRaises(ValueError):
                validate(value)

    def test_removing_an_ignore_attribute_cannot_reclassify_a_measurement_silently(self):
        value = self.fixture()
        source = (ROOT / "crates/platform/dispatch/awaken-dispatch/tests/dispatch_claim_perf.rs").read_text(encoding="utf-8")
        original = '#[ignore = "requires a live PostgreSQL fixture and intentionally creates a large queue"]'
        self.assertIn(original, source)
        with self.assertRaisesRegex(ValueError, "ignored classification"):
            validate_scale_inventory(value, source.replace(original, "", 1))

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
        for paths, layers, cold, scale in [
            (["docs/design/test-validation.md"], {"L0"}, False, False),
            (["crates/platform/dispatch/awaken-dispatch/src/lease.rs"], {"L0","L1","L2","L3","L4"}, False, True),
            (["crates/platform/dispatch/awaken-dispatch/Cargo.toml"], {"L0","L1","L2","L3","L4"}, True, True),
            (["e2e/harness.mjs"], {"L0","L2","L3"}, False, False),
            (["crates/platform/dispatch/awaken-dispatch/src/postgres_claim.rs"], {"L0","L1","L2","L3","L4"}, False, True),
            (["new-build-input"], {"L0","L1","L2","L3","L4"}, True, True),
        ]:
            with self.subTest(paths=paths):
                result = required_layers(paths, metadata=self.metadata())
                self.assertEqual(set(result["required_layers"]), layers)
                self.assertEqual((result["cold_required"], result["scale_required"]), (cold, scale))
                self.assertIs(result["full_gate"], False)
                self.assertIs(result["execution_performed"], False)
        self.assertEqual(required_layers([], qualification=True)["required_layers"], ["L0","L1","L2","L3","L4"])
        mixed = required_layers(["docs/a.md", "Cargo.lock"], metadata=self.metadata())
        self.assertEqual(mixed["required_layers"], ["L0","L1","L2","L3","L4"])
        self.assertTrue(mixed["scale_required"])
        for path in ("../outside", "/absolute", "C:/absolute"):
            with self.assertRaises(ValueError):
                required_layers([path], metadata=self.metadata())


    def test_external_rust_fixture_obligations_use_canonical_gate_inputs(self):
        from unittest import mock
        for path in ("e2e/fixtures/acp_awaken_tool_bridge_fixture.mjs",
                     "e2e/fixtures/mcp_http_fixture_client.mjs", "e2e/fixtures/future_fixture.mjs"):
            for metadata in (None, self.metadata()):
                with self.subTest(path=path, graph=metadata is not None):
                    result = required_layers([path], metadata=metadata)
                    self.assertEqual(result["required_layers"], ["L0", "L1", "L2", "L3", "L4"])
                    self.assertTrue(result["cold_required"] and result["scale_required"])
        # The owner of the Rust gate input declaration, not a second path list,
        # also controls a future external path under the process fixture tree.
        path = "e2e/new_asset.data"
        with mock.patch("rust_test_plan.gate_inputs", return_value=("e2e/new_asset.data",)):
            self.assertIn("L1", required_layers([path], metadata=self.metadata())["required_layers"])
        self.assertEqual(required_layers(["e2e/scenario.mjs"], metadata=self.metadata())["required_layers"], ["L0", "L2", "L3"])

    def metadata(self):
        entries = [
            ("work", "awaken-work-store", "crates/platform/environment/awaken-work-store", []),
            ("runtime", "awaken-runtime", "crates/sdk/runtime/awaken-runtime", []),
            ("kit", "awaken-dispatch-testkit", "crates/platform/dispatch/awaken-dispatch-testkit", ["work", "runtime"]),
            ("dispatch", "awaken-dispatch", "crates/platform/dispatch/awaken-dispatch", ["kit"]),
            ("unrelated", "unrelated-plugin", "sdk/unrelated-plugin", []),
        ]
        return {
            "workspace_members": [row[0] for row in entries],
            "packages": [{"id": key, "name": name, "manifest_path": str(ROOT / directory / "Cargo.toml")}
                         for key, name, directory, _ in entries],
            "resolve": {"nodes": [{"id": key, "deps": [{"pkg": dep} for dep in deps]}
                                  for key, _, _, deps in entries]},
        }

    def test_moved_work_authority_and_embedded_readme_use_the_real_consumers(self):
        for path in ("crates/platform/environment/awaken-work-store/src/lease_book.rs",
                     "crates/platform/environment/awaken-work-store/src/schema.rs",
                     "crates/sdk/runtime/awaken-runtime/README.md"):
            with self.subTest(path=path):
                result = required_layers([path], metadata=self.metadata())
                self.assertTrue({"L1", "L2", "L3", "L4"} <= set(result["required_layers"]))
                self.assertTrue(result["scale_required"])
                self.assertFalse(result["cold_required"])
        # A newly organized SDK directory stays owned, without another path list.
        unrelated = required_layers(["sdk/unrelated-plugin/src/lib.rs"], metadata=self.metadata())
        self.assertEqual(unrelated["required_layers"], ["L0", "L1", "L2", "L3"])
        self.assertFalse(unrelated["scale_required"])

    def test_scale_obligation_follows_dependency_edges_not_an_old_directory_label(self):
        metadata = self.metadata()
        for node in metadata["resolve"]["nodes"]:
            if node["id"] == "kit":
                node["deps"] = [{"pkg": "runtime"}]
        result = required_layers(["crates/platform/environment/awaken-work-store/src/lease_book.rs"], metadata=metadata)
        self.assertFalse(result["scale_required"])
        result = required_layers(["crates/sdk/runtime/awaken-runtime/src/lib.rs"], metadata=metadata)
        self.assertTrue(result["scale_required"])

    def test_missing_graph_and_nonprose_docs_never_claim_narrow_rust_qualification(self):
        for path in ("crates/platform/environment/awaken-work-store/src/lease_book.rs",
                     "crates/sdk/runtime/awaken-runtime/README.md", "docs/unknown-input.json"):
            result = required_layers([path])
            self.assertEqual(result["required_layers"], ["L0", "L1", "L2", "L3", "L4"])
            self.assertTrue(result["cold_required"])
            self.assertTrue(result["scale_required"])
        broken = self.metadata()
        broken["resolve"] = None
        with self.assertRaises(ValueError):
            required_layers(["crates/platform/environment/awaken-work-store/src/schema.rs"], metadata=broken)

    def test_plan_cli_uses_the_canonical_metadata_reader_and_rejects_a_source_race(self):
        import contextlib
        import io
        from unittest import mock
        paths = ["crates/platform/environment/awaken-work-store/src/schema.rs"]
        for changed in (False, True):
            output = io.StringIO()
            with self.subTest(changed=changed), mock.patch("rust_test_plan.changed_sources", return_value=paths), \
                    mock.patch("rust_test_plan.working_subject", side_effect=[("before", {}), ("after" if changed else "before", {})]), \
                    mock.patch("crate_test.load_profile", return_value=(["fixture/test-support"], [])), \
                    mock.patch("crate_test.environment", return_value={}), \
                    mock.patch("crate_test.full_graph_metadata", return_value=self.metadata()) as metadata, \
                    contextlib.redirect_stdout(output):
                if changed:
                    with self.assertRaisesRegex(ValueError, "source changed"):
                        main(["--plan-base", "fixture-base"])
                    self.assertEqual(output.getvalue(), "")
                else:
                    self.assertEqual(main(["--plan-base", "fixture-base"]), 0)
                    observed = json.loads(output.getvalue())
                    self.assertTrue(observed["scale_required"])
                    self.assertFalse(observed["execution_performed"])
                    self.assertEqual(metadata.call_count, 1)




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
        from crate_test import environment, full_graph_metadata, load_profile
        before,_=working_subject();paths=changed_sources(args.plan_base)
        features, _ = load_profile(ROOT / ".config/rust-tests.toml")
        metadata = full_graph_metadata(features, root=ROOT, environment=environment(ROOT), host_only=False)
        plan=required_layers(paths,qualification=args.qualification,metadata=metadata,root=ROOT)
        if working_subject()[0]!=before:raise ValueError("source changed during layer planning")
        print(json.dumps(dict(plan,source_digest=before,changed_paths=paths),indent=2));return 0
    if args.qualification:parser.error("--qualification requires --plan-base")
    value = json.loads((ROOT / MATRIX).read_text(encoding="utf-8")); validate(value)
    print(f"Source-design test layer matrix: {len(value['rows'])} risk domains; source links valid; execution remains deferred")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
