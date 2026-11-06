#!/usr/bin/env python3
"""Execute the Session acceptance gates' negative controls on real inputs.

The static gate scenarios (boundary, impact selection, list/execute alignment
and evidence validity) are only proven when the production guard refuses a
real counter-example. Each test below builds the guard's input from the actual
workspace or ledger, proves the unmodified input is accepted, then injects one
violation and requires the refusal the guard owns. A guard that stops checking
the invariant makes the matching test fail.
"""

from __future__ import annotations

import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from collections import Counter, deque
from functools import lru_cache
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import _architecture_ratchet  # noqa: E402
import _contract_map_fitness  # noqa: E402
import _crate_boundary_workspace  # noqa: E402
import _crate_dependency_fitness  # noqa: E402
import _session_state_ownership_fitness  # noqa: E402
import check_rust_test_execution  # noqa: E402
import check_session_acceptance  # noqa: E402
import formal_sources  # noqa: E402
import rust_test_plan  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
# What these gate tests read: the workspace sources and manifests the guards
# measure, the ledger, and the guards themselves.
INPUT_GLOBS: tuple[str, ...] = (
    ".gitattributes",
    "Cargo.toml", "Cargo.lock", "crates/**/*.rs", "crates/**/Cargo.toml",
    "docs/design/session-execution-test-acceptance/**", "scripts/ci/*.py", "scripts/ci/*.json",
    "examples/**", "e2e/**", "rust-toolchain.toml", ".cargo/**", "scripts/ci/*.sh", "packages/managed-sdk-oracle/package.json",
)
SHARED_CONTRACT = "crates/platform/environment/awaken-provisioning-contract/src/lib.rs"
OWNED_STATE_FILE = "crates/platform/protocols/awaken-protocol-managed/src/lib.rs"


@lru_cache(maxsize=1)
def workspace_metadata() -> dict:
    output = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"],
                            cwd=ROOT, capture_output=True, check=True)
    return json.loads(output.stdout)


def reverse_dependants(metadata: dict, package: str) -> set[str]:
    """An independent oracle: every member that reaches ``package`` in Cargo's resolve."""
    members = set(metadata["workspace_members"])
    names = {p["id"]: p["name"] for p in metadata["packages"] if p["id"] in members}
    consumers: dict[str, set[str]] = {identity: set() for identity in members}
    for node in metadata["resolve"]["nodes"]:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    consumers[dependency["pkg"]].add(node["id"])
    start = next(identity for identity, name in names.items() if name == package)
    found, pending = {start}, deque([start])
    while pending:
        for consumer in consumers[pending.popleft()] - found:
            found.add(consumer)
            pending.append(consumer)
    return {names[identity] for identity in found}


class BoundaryGateTests(unittest.TestCase):
    """Declarations, aliases and wrappers of Host/MCP/Environment/Memory."""

    def test_out_of_bound_dependency_into_host_mcp_environment_memory_is_refused(self) -> None:
        specs = _crate_boundary_workspace.dependency_fitness_specs()
        self.assertEqual(_crate_dependency_fitness.dependency_violations(specs), [])
        by_name = {spec.name: spec for spec in specs}
        # Each edge reaches a wider owner than its layer/context admits.
        for source, target in (("awaken-memory-store", "awaken-session-application"),
                               ("awaken-env-store", "awaken-session-application"),
                               ("awaken-session-mcp", "awaken-session-application")):
            with self.subTest(edge=(source, target)):
                self.assertIn(target, by_name)
                mutated = [spec._replace(normal_deps=spec.normal_deps | {target}) if spec.name == source else spec
                           for spec in specs]
                errors = _crate_dependency_fitness.dependency_violations(mutated)
                self.assertTrue(any(error.startswith(f"{source} (") and f"depends on {target} (" in error
                                    for error in errors), errors)

    def _contract_counts(self, edit) -> dict:
        sources = list(_architecture_ratchet.crate_sources(_crate_boundary_workspace.CRATES))
        edited = []
        for crate in sources:
            files = dict(crate.files)
            if crate.name == "awaken-provisioning-contract":
                edit(crate.directory, files)
            edited.append(_architecture_ratchet.CrateSource(crate.name, crate.metadata, crate.directory, files))
        counts, _places, _contracts = _contract_map_fitness.measure(tuple(edited), _crate_boundary_workspace.REPO_ROOT)
        return counts

    def test_alias_or_moved_wrapper_cannot_hide_a_wide_object(self) -> None:
        wide = "pub use awaken_session_executor::SessionExecutor as NarrowHost;\n"

        def unchanged(_directory, _files) -> None:
            return None

        def in_lib(directory, files) -> None:
            files[directory / "src/lib.rs"] = files[directory / "src/lib.rs"] + "\n" + wide

        def moved(directory, files) -> None:
            files[directory / "src/moved/narrow.rs"] = wide

        base = self._contract_counts(unchanged)
        crate = "awaken-provisioning-contract"
        for edit in (in_lib, moved):
            with self.subTest(edit=edit.__name__):
                counts = self._contract_counts(edit)
                for rule in ("contract-reexport", "renamed-reexport"):
                    self.assertEqual(counts[rule].get(crate, 0), base[rule].get(crate, 0) + 1, rule)
                baseline = json.loads(_contract_map_fitness.BASELINE.read_text(encoding="utf-8"))
                errors = _architecture_ratchet.compare(_contract_map_fitness.RULES, counts, {}, baseline, "baseline")
                self.assertTrue(any(error.startswith("renamed-reexport: " + crate) for error in errors), errors)

    def test_state_owner_write_through_a_wrapper_module_is_refused(self) -> None:
        sources = {
            _crate_boundary_workspace.repository_path(path, ROOT): path.read_text(encoding="utf-8")
            for path in sorted((ROOT / "crates").glob("**/src/**/*.rs"))
        }
        self.assertEqual(_session_state_ownership_fitness.session_state_ownership_violations(sources), [])
        bypass = "\nfn restore(mut persisted: Persisted) { persisted.execution = Default::default(); }\n"
        for path in (OWNED_STATE_FILE, "crates/platform/protocols/awaken-protocol-managed/src/moved/wrapper.rs"):
            with self.subTest(path=path):
                mutated = dict(sources)
                mutated[path] = mutated.get(path, "") + bypass
                errors = _session_state_ownership_fitness.session_state_ownership_violations(mutated)
                self.assertTrue(any(error.startswith(path + ": direct Session state write") for error in errors), errors)

    def test_every_group_owner_entrypoint_is_traceable_and_a_move_is_refused(self) -> None:
        data = check_session_acceptance._load(ROOT)
        self.assertEqual(check_session_acceptance.ledger_errors(data, ROOT), [])
        for group in ("T01", "T16", "T20", "T22"):
            with self.subTest(group=group):
                moved = copy.deepcopy(data)
                target = next(g for g in moved["groups"] if g["id"] == group)
                target["entrypoint"] = target["entrypoint"] + ".moved"
                errors = check_session_acceptance.ledger_errors(moved, ROOT)
                self.assertIn(f"L02 {group} entrypoint {target['entrypoint']} is absent", errors)


class ImpactSelectionTests(unittest.TestCase):
    """A shared input selects its real reverse dependants; unknown input widens."""

    def test_shared_contract_change_selects_every_real_reverse_dependant(self) -> None:
        metadata = workspace_metadata()
        expected = reverse_dependants(metadata, "awaken-provisioning-contract")
        selected, reason = rust_test_plan.affected_packages(metadata, [SHARED_CONTRACT], ROOT)
        self.assertEqual(set(selected), expected)
        self.assertIn("transitive consumers", reason)
        # The contract change must reach the Session owners that construct it.
        for consumer in ("awaken-session-application", "awaken-session-executor", "awaken-sandbox-container"):
            self.assertIn(consumer, selected)

    def test_crate_only_selection_is_detected_as_too_narrow(self) -> None:
        metadata = workspace_metadata()
        expected = reverse_dependants(metadata, "awaken-provisioning-contract")
        # The mutant planner forgets every edge, i.e. selects by file location.
        mutant = copy.deepcopy(metadata)
        for node in mutant["resolve"]["nodes"]:
            node["deps"] = []
        narrowed, _reason = rust_test_plan.affected_packages(mutant, [SHARED_CONTRACT], ROOT)
        self.assertEqual(narrowed, ["awaken-provisioning-contract"])
        self.assertNotEqual(set(narrowed), expected)
        self.assertGreater(len(expected), len(narrowed))

    def test_unknown_or_global_input_widens_and_never_selects_zero(self) -> None:
        metadata = workspace_metadata()
        members = {p["name"] for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
        for path in ("crates/unregistered-crate/src/lib.rs", "newtool/main.rs", "Cargo.lock",
                     ".config/rust-tests.toml", "scripts/ci/crate_test.py",
                     "crates/platform/environment/awaken-provisioning-contract/Cargo.toml"):
            with self.subTest(path=path):
                selected, reason = rust_test_plan.affected_packages(metadata, [path], ROOT)
                self.assertEqual(set(selected), members)
                self.assertTrue(reason.startswith(("global or unowned input", "manifest input")), reason)
        # A change that owns no Rust boundary is an explicit no-work verdict,
        # never a green full-gate result.
        selected, reason = rust_test_plan.affected_packages(metadata, ["docs/design/notes.md"], ROOT)
        self.assertEqual(selected, [])
        self.assertIn("not a full-gate result", reason)
        scope, _selected, reason = rust_test_plan.gate_scope(metadata, ["crates/unregistered-crate/src/lib.rs"], ROOT)
        self.assertEqual(scope, "whole", reason)


class ListedExecutedTests(unittest.TestCase):
    """Required execution is aligned by name and occurrence, not by total."""

    INVENTORY = "tests::one: test\ntests::two: test\n2 tests, 0 benchmarks\n"
    OUTPUT = "test tests::one ... ok\ntest tests::two ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n"

    @staticmethod
    def count_only(inventory: str, results: str, exit_code: int) -> list[str]:
        """The negative control: a validator that compares totals only."""
        listed = len(check_rust_test_execution.LISTED.findall(inventory))
        executed = len(check_rust_test_execution.EXECUTED.findall(results))
        return [] if listed and listed == executed and not exit_code else ["count differs"]

    def test_each_misalignment_is_refused_for_its_own_reason(self) -> None:
        evidence = check_rust_test_execution.execution_evidence
        self.assertEqual(evidence(self.INVENTORY, self.OUTPUT, 0)["errors"], [])
        rows = (
            ("zero cases", "", self.OUTPUT, 0, "required selection is empty"),
            ("renamed", self.INVENTORY, self.OUTPUT.replace("tests::two ...", "tests::renamed ..."), 0, "selection differs"),
            ("duplicate occurrence", self.INVENTORY, self.OUTPUT + "test tests::one ... ok\n", 0, "selection differs"),
            ("ignored", self.INVENTORY, self.OUTPUT.replace("two ... ok", "two ... ignored"), 0, "failed or were ignored"),
            ("fixture skip", self.INVENTORY, self.OUTPUT + "SKIPPED PostgreSQL fixture: absent\n", 0, "skipped its fixture"),
            ("child exit", self.INVENTORY, self.OUTPUT, 101, "test command failed with status 101"),
        )
        for label, inventory, output, status, reason in rows:
            with self.subTest(label=label):
                errors = evidence(inventory, output, status)["errors"]
                self.assertTrue(any(reason in error for error in errors), errors)

    def test_count_only_validator_accepts_what_name_alignment_refuses(self) -> None:
        renamed = self.OUTPUT.replace("tests::two ...", "tests::renamed ...")
        self.assertEqual(self.count_only(self.INVENTORY, renamed, 0), [])
        errors = check_rust_test_execution.execution_evidence(self.INVENTORY, renamed, 0)["errors"]
        self.assertTrue(any(error.startswith("selection differs") for error in errors), errors)
        executed = Counter(name for name, _ in check_rust_test_execution.EXECUTED.findall(renamed))
        self.assertNotEqual(executed, Counter(check_rust_test_execution.LISTED.findall(self.INVENTORY)))


class EvidenceValidityTests(unittest.TestCase):
    """Ledger bindings must cite assertions, executed slices and original receipts."""

    def refused(self, code: str, mutate) -> list[str]:
        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = check_session_acceptance._fixture(root)
            frozen = {e["id"]: check_session_acceptance.entry_digest(e) for e in base["evidence_registry"]}
            self.assertEqual(check_session_acceptance.ledger_errors(base, root, frozen), [])
            data = copy.deepcopy(base)
            mutate(data)
            errors = check_session_acceptance.ledger_errors(data, root, frozen)
        self.assertTrue(any(error.startswith(code) for error in errors), errors)
        return errors

    def test_name_only_or_zero_assertion_binding_cannot_sign_off(self) -> None:
        def name_only(data):
            binding = data["bindings"][0]
            binding.update(name="named_only", assertion_locations=[])
            binding["source_locations"][0]["line"] = 5

        def construction_only(data):
            name_only(data)
            data["bindings"][0]["assertion_locations"] = [{"path": "src/lib.rs", "line": 6}]

        def unexecuted(data):
            data["bindings"][0]["execution_witnesses"] = []

        self.refused("L04", name_only)
        self.refused("L04", construction_only)
        self.refused("L05", unexecuted)

    def test_unrun_slice_or_compile_only_run_cannot_be_promoted(self) -> None:
        def compile_only(data):
            entry = data["evidence_registry"][0]
            entry["argv"] = ["cargo", "test", "--no-run"]
            entry["entry_sha256"] = check_session_acceptance.entry_digest(entry)

        def ignored(data):
            data["bindings"][0]["execution_witnesses"][0]["skipped"] = True

        def no_slices(data):
            data["groups"][0]["cases"][0]["slices"] = {"executed": ["windows"]}

        def accepted_with_open_slice(data):
            data["groups"][0]["cases"][0].update(acceptance_state="accepted_in_scope", acceptance_scope="windows")
            data["catalog"]["counts"]["fully_accepted_scenarios"] = 1

        self.refused("L05", compile_only)
        self.refused("L05", ignored)
        self.refused("L08", no_slices)
        self.refused("L09", accepted_with_open_slice)

    def test_historical_receipt_relabelled_as_current_is_refused(self) -> None:
        def relabel(data):
            entry = data["evidence_registry"][0]
            entry["source"]["head"] = "b" * 40
            data["bindings"][0]["execution_witnesses"][0]["source_head"] = "b" * 40
            entry["entry_sha256"] = check_session_acceptance.entry_digest(entry)

        def edit_in_place(data):
            data["evidence_registry"][0]["source"]["head"] = "c" * 40

        def inherit_unreviewed(data):
            entry = data["evidence_registry"][0]
            entry["historical"] = True
            data["bindings"][0]["execution_witnesses"][0]["historical"] = True
            entry["entry_sha256"] = check_session_acceptance.entry_digest(entry)

        errors = self.refused("L06", relabel)
        self.assertTrue(any(error.startswith("L11") for error in errors), errors)
        self.refused("L11", edit_in_place)
        self.refused("L06", inherit_unreviewed)

    def test_duplicate_identities_are_refused_before_dictionary_lookup(self) -> None:
        for collection in ("groups", "bindings", "evidence_registry"):
            for changed in (False, True):
                with self.subTest(collection=collection, changed=changed):
                    def collide(data):
                        duplicate = copy.deepcopy(data[collection][0])
                        if changed:
                            duplicate["collision_control"] = "independent registration"
                        data[collection].append(duplicate)
                    errors = self.refused("L13", collide)
                    self.assertEqual(len(errors), 1)
                    self.assertIn(collection, errors[0])

        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = check_session_acceptance._fixture(root)
            independent = copy.deepcopy(data["evidence_registry"][0])
            independent["id"] = "E91"
            independent["entry_sha256"] = check_session_acceptance.entry_digest(independent)
            data["evidence_registry"].append(independent)
            self.assertEqual(check_session_acceptance.ledger_errors(data, root), [])

    def test_committed_ledger_satisfies_every_rule(self) -> None:
        data = check_session_acceptance._load(ROOT)
        frozen = check_session_acceptance.committed_registry(ROOT)
        self.assertEqual(check_session_acceptance.ledger_errors(data, ROOT, frozen), [])
        self.assertEqual(check_session_acceptance.self_test(), 0)


class EvidenceArtifactByteTests(unittest.TestCase):
    """Native Git must store the same raw evidence bytes the registry hashes."""

    def git(self, root: Path, *arguments: str) -> bytes:
        return subprocess.check_output(
            ["git", *arguments], cwd=root,
            env=formal_sources.detached_git_environment(), stderr=subprocess.PIPE,
        )

    def assert_raw_round_trip(self, attributes: bytes, payload: bytes, automatic: str) -> None:
        with tempfile.TemporaryDirectory(prefix="acceptance-evidence-bytes-") as raw:
            root = Path(raw)
            self.git(root, "init", "--quiet")
            self.git(root, "config", "core.autocrlf", automatic)
            (root / ".gitattributes").write_bytes(attributes.replace(b"\r\n", b"\n"))
            relative = "docs/design/session-execution-test-acceptance/evidence/fixture/receipt.json"
            receipt = root / relative
            receipt.parent.mkdir(parents=True)
            receipt.write_bytes(payload)
            self.git(root, "add", "--", ".gitattributes", relative)
            self.assertEqual(self.git(root, "show", ":" + relative), payload,
                             "staged evidence must retain the hashed bytes")
            self.git(root, "diff", "--cached", "--check", "--", relative)
            for checkout_policy in ("false", "true"):
                self.git(root, "config", "core.autocrlf", checkout_policy)
                self.git(root, "checkout-index", "--force", "--", relative)
                self.assertEqual(receipt.read_bytes(), payload,
                                 "checkout policy must not rewrite registered evidence")
            # CRLF is valid raw data; ordinary trailing spaces still fail the
            # unchanged whitespace gate instead of acquiring a broad waiver.
            receipt.write_bytes(payload.rstrip(b"\r\n") + b" \r\n")
            self.git(root, "add", "--", relative)
            with self.assertRaises(subprocess.CalledProcessError):
                self.git(root, "diff", "--cached", "--check", "--", relative)

    def test_raw_evidence_survives_native_git_staging_and_both_checkout_policies(self) -> None:
        attributes = (ROOT / ".gitattributes").read_bytes()
        for automatic in ("false", "true"):
            for newline in (b"\n", b"\r\n"):
                with self.subTest(staging=automatic, newline=newline):
                    self.assert_raw_round_trip(
                        attributes, newline.join((b"{", b'  "passed": true', b"}", b"")), automatic,
                    )

    def test_removing_raw_evidence_policy_reproduces_the_hash_failure(self) -> None:
        attributes = (ROOT / ".gitattributes").read_bytes()
        rule = b"docs/design/session-execution-test-acceptance/evidence/** -text whitespace=cr-at-eol"
        self.assertEqual(attributes.count(rule), 1)
        # The same staging/checkout oracle must detect real Git rewriting, not
        # a fabricated digest or an unrelated missing-file/configuration fault.
        with self.assertRaisesRegex(AssertionError, "staged evidence must retain"):
            self.assert_raw_round_trip(attributes.replace(rule, b""), b'{\r\n  "passed": true\r\n}\r\n', "true")

    def test_independent_e42_registration_keeps_its_original_source_and_receipt(self) -> None:
        entries = {entry["id"]: entry for entry in check_session_acceptance._load(ROOT)["evidence_registry"]}
        directory = ROOT / "docs/design/session-execution-test-acceptance/evidence/E42-materializer-revocation-windows"
        original = json.loads((directory / "original-registry-entry.json").read_text(encoding="utf-8"))
        resolution = json.loads((directory / "registry-name-resolution.json").read_text(encoding="utf-8"))
        self.assertEqual(original["id"], "E42")
        self.assertEqual(original["entry_sha256"], check_session_acceptance.entry_digest(original))
        self.assertEqual(resolution["original_entry_sha256"], original["entry_sha256"])
        self.assertEqual(resolution["catalog_entry_sha256"], entries["E42W"]["entry_sha256"])
        self.assertEqual(resolution["receipt_sha256"], original["receipt_sha256"])
        self.assertEqual({k: v for k, v in original.items() if k not in ("id", "entry_sha256")},
                         {k: v for k, v in entries["E42W"].items() if k not in ("id", "entry_sha256")})
        self.assertNotEqual(entries["E42"]["receipt_sha256"], entries["E42W"]["receipt_sha256"])
        self.assertEqual(entries["E42W"]["platform"], "local Windows x86_64-pc-windows-msvc")

    def test_registered_portable_artifacts_are_stored_with_their_declared_hashes(self) -> None:
        entries = check_session_acceptance._load(ROOT)["evidence_registry"]
        portable = [entry for entry in entries if entry.get("receipt", "").startswith(
            "docs/design/session-execution-test-acceptance/evidence/")]
        self.assertTrue(portable, "portable evidence must have an indexed byte check")
        for entry in portable:
            receipt = entry["receipt"]
            with self.subTest(evidence=entry["id"], artifact=receipt):
                indexed = formal_sources.git_output(ROOT, "show", ":" + receipt)
                self.assertEqual(hashlib.sha256(indexed).hexdigest(), entry["receipt_sha256"])
                self.assertEqual(indexed, (ROOT / receipt).read_bytes())
            proof = json.loads(indexed)
            directory = Path(receipt).parent
            for name, digest in proof.get("raw_sha256", {}).items():
                self.assertEqual(Path(name).name, name, "raw artifact names must stay within their receipt")
                relative = (directory / name).as_posix()
                with self.subTest(evidence=entry["id"], artifact=relative):
                    indexed = formal_sources.git_output(ROOT, "show", ":" + relative)
                    self.assertEqual(hashlib.sha256(indexed).hexdigest(), digest)
                    self.assertEqual(indexed, (ROOT / relative).read_bytes())


if __name__ == "__main__":
    unittest.main()
