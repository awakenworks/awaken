#!/usr/bin/env python3
"""Focused tests for the formal production-surface inventory boundary."""

from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("check_formal_surface.py")
SPEC = importlib.util.spec_from_file_location("formal_surface", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FormalSurfaceProductionSourceTests(unittest.TestCase):
    def test_each_surface_has_one_classification_even_when_owners_agree(self) -> None:
        # C1 one reachable source; C2 a second classification for that source;
        # C3 the duplicate names the same or a different valid requirement.
        # R1 C1+!C2 -> one owner; R2 C1+C2+same -> reject redundancy;
        # R3 C1+C2+different -> reject competing owners, never last-writer-wins.
        # Removal of only the duplicate restores R1 without increasing proof grade.
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "policy.rs").write_text("fn decide() {}")
            classifications = root / "classifications.json"
            first = {"path": "policy.rs", "requirement_id": "domain", "reason": "owner"}
            with mock.patch.multiple(MODULE, ROOT=root, CLASSIFICATIONS=classifications):
                for second_owner in [None, "domain", "protocol", None]:
                    entries = [first]
                    if second_owner is not None:
                        entries.append({**first, "requirement_id": second_owner})
                    classifications.write_text(json.dumps({
                        "version": 1, "classifications": entries,
                    }))
                    if second_owner is None:
                        self.assertEqual(MODULE.load_classifications(set(), {"domain", "protocol"}),
                                         {"policy.rs": first}, "R1")
                    else:
                        message = io.StringIO()
                        with contextlib.redirect_stderr(message), self.assertRaises(SystemExit) as error:
                            MODULE.load_classifications(set(), {"domain", "protocol"})
                        self.assertEqual(error.exception.code, 1, "R2/R3")
                        self.assertIn("duplicate surface classification policy.rs", message.getvalue())

    def test_explicit_targets_survive_disabled_cargo_auto_discovery(self) -> None:
        # T1 auto roots disabled -> conventional unregistered lib/bin files do
        # not earn evidence. T2 explicit custom library/binary paths and named
        # binaries -> reachable, including child modules beside custom roots.
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text(
                '[package]\nname="fixture"\nversion="0.0.0"\nautolib=false\nautobins=false\n'
                '[[bin]]\nname="explicit"\npath="entry/serve.rs"\n'
                '[[bin]]\nname="named"\n'
            )
            for relative in ["src/lib.rs", "src/main.rs", "src/bin/orphan.rs", "src/bin/named.rs", "entry/serve.rs", "entry/root.rs", "entry/child.rs"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("mod child;" if relative == "entry/root.rs" else "")
            linked = MODULE._crate_boundary_workspace.production_module_files([manifest])
            self.assertEqual({p.relative_to(root).as_posix() for p in linked}, {"entry/serve.rs", "src/bin/named.rs"}, "T1")
            with manifest.open("a") as output:
                output.write('[lib]\npath="entry/root.rs"\n')
            linked = MODULE._crate_boundary_workspace.production_module_files([manifest])
            self.assertEqual({p.relative_to(root).as_posix() for p in linked}, {"entry/serve.rs", "src/bin/named.rs", "entry/root.rs", "entry/child.rs"}, "T2")

    def test_ledger_cannot_credit_a_file_after_its_module_edge_is_removed(self) -> None:
        # Causes L1 the ledger points to an existing reachable production module;
        # L2 only its mod declaration is removed; L3 an exclusion attempts to
        # disguise the same proof as test-only. Effects: L1 passes; L2/L3 reject
        # the actual strict checker, even though the evidence file still exists.
        # L4 a direct ledger link gains a redundant surface classification ->
        # reject that exact duplicate; removing only the duplicate passes again.
        # This is the transition when a previously classified file gains a
        # production proof, as with the credential secret boundary after rebase.
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            crate = root / "crates/runtime/example"
            (crate / "src").mkdir(parents=True)
            manifest = crate / "Cargo.toml"
            manifest.write_text('[package]\nname="example"\nversion="0.0.0"\n')
            (crate / "src/lib.rs").write_text("mod policy;")
            policy = "crates/runtime/example/src/policy.rs"
            (root / policy).write_text("fn authorize() {}")
            MODULE.formal_ledger.write(root, {"obligations": [{
                "id": "example", "status": "model_linked", "evidence": [policy],
            }]})
            features = root / "features.json"
            features.write_text('{"features":[]}')
            exclusions = root / "exclusions.json"
            with (
                mock.patch.multiple(
                    MODULE, ROOT=root, FEATURES=features,
                    EXCLUSIONS=exclusions, CLASSIFICATIONS=root / "classifications.json",
                    PRODUCTION_ROOTS=[root / "crates"],
                ),
                mock.patch.object(MODULE._crate_boundary_workspace, "iter_crate_manifests", return_value=[manifest]),
                mock.patch("sys.argv", ["check_formal_surface.py", "--require-complete"]),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                MODULE.main()
                classifications = root / "classifications.json"
                classifications.write_text(json.dumps({"version": 1, "classifications": [{
                    "path": policy, "obligation_ids": ["example"], "reason": "redundant owner",
                }]}))
                message = io.StringIO()
                with contextlib.redirect_stderr(message), self.assertRaises(SystemExit) as error:
                    MODULE.main()
                self.assertEqual(error.exception.code, 1, "L4")
                self.assertIn("classifications duplicate an exclusion or source-ledger link", message.getvalue(), "L4")
                classifications.unlink()
                MODULE.main()
                (crate / "src/lib.rs").write_text("")
                with self.assertRaises(SystemExit) as error:
                    MODULE.main()
                self.assertEqual(error.exception.code, 1, "L2")
                exclusions.write_text(json.dumps({"exclusions": [{
                    "path": policy, "boundary": "test_only", "reason": "invalid camouflage",
                }]}))
                with self.assertRaises(SystemExit) as error:
                    MODULE.main()
                self.assertEqual(error.exception.code, 1, "L3")

    def test_module_links_exclude_detached_and_test_only_evidence(self) -> None:
        # Causes: C1 a Cargo target reaches ordinary/inline/path/include modules;
        # C2 the only edge is cfg(test); C3 an edge is removed but its file stays;
        # C4 a feature/platform branch may compile in a production profile.
        # Effects: M1 C1/C4 -> structurally linked; M2 C2/C3 -> no evidence.
        # A source string/comment mentioning a module is not an edge (M3).
        # Constraint: this is a union of non-test configurations, not a compiler
        # or macro-expansion proof; release builds retain cfg/target authority.
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text('[package]\nname = "fixture"\nversion = "0.0.0"\n')
            files = {
                "src/lib.rs": (
                    'mod ordinary; mod inline { mod nested; }\n'
                    '#[path = "renamed.rs"] mod alias;\n'
                    'include!("included.rs");\n'
                    '#[cfg(test)] mod tests;\n'
                    '#[cfg(kani)] mod proofs;\n'
                    '#[cfg(any(test, kani))] mod model;\n'
                    '#[cfg(any(test, feature = "support"))] mod feature;\n'
                    '// mod orphan;\nconst TEXT: &str = "mod orphan;";\n'
                ),
                "src/ordinary.rs": "mod child;",
                "src/ordinary/child.rs": "",
                "src/inline/nested.rs": "",
                "src/renamed.rs": "",
                "src/included.rs": "",
                "src/tests.rs": "mod fake;",
                "src/tests/fake.rs": "",
                "src/feature.rs": "",
                "src/proofs.rs": "",
                "src/model.rs": "",
                "src/orphan.rs": "fn authorize() {}",
                "src/main.rs": "",
            }
            for relative, source in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(source)
            linked = MODULE._crate_boundary_workspace.production_module_files([manifest])
            verification = {"src/tests.rs", "src/tests/fake.rs", "src/proofs.rs", "src/model.rs"}
            expected = set(files) - verification - {"src/orphan.rs"}
            self.assertEqual({p.relative_to(root).as_posix() for p in linked}, expected, "M1-M3")
            # M4 a module gated on kani, or on test and kani together, is not
            # shipped, but it is compiled; the compiled graph attaches it, while
            # a file no build declares stays detached from both graphs.
            compiled = MODULE._crate_boundary_workspace.compiled_module_files([manifest])
            self.assertEqual(
                {p.relative_to(root).as_posix() for p in compiled},
                expected | verification,
                "M4 compiled graph = production + test + kani modules, never the orphan",
            )
            (root / "src/lib.rs").write_text("mod ordinary;")
            linked = MODULE._crate_boundary_workspace.production_module_files([manifest])
            self.assertNotIn(root / "src/renamed.rs", linked, "M2 removed edge")

    def test_only_production_modules_enter_the_surface_denominator(self) -> None:
        # Cause/effect decision table:
        # P1 crates/*/src/lib.rs -> production surface; P2 tests.rs/test.rs,
        # *_tests.rs, or a tests/ directory -> test evidence only; P3 a Rust
        # file outside src -> not production. Effect E1: only P1 enters the
        # source-oriented denominator, so tests cannot duplicate their owner.
        for relative, expected in [
            ("crates/example/src/lib.rs", True),
            ("crates/example/src/tests.rs", False),
            ("crates/example/src/test.rs", False),
            ("crates/example/src/recovery_tests.rs", False),
            ("crates/example/src/tests/recovery.rs", False),
            ("crates/example/tests/recovery.rs", False),
        ]:
            self.assertEqual(
                MODULE.is_production_source(MODULE.ROOT / relative),
                expected,
                relative,
            )

    def test_signal_projection_distinguishes_url_comments_and_cfg_items(self) -> None:
        # Causes: C1 a URL string contains `//`; C2 executable authorization
        # follows it; C3 authorization occurs only in a line/block comment or
        # cfg(test) item; C4 cfg(any(test, feature)) can compile outside tests.
        # Effects: S1 C1+C2 and C4 retain the signal; S2 C3 is ignored. These
        # rules bind the production denominator to the shared Rust projection.
        url_then_live = (
            'const URL: &str = "https://formal.invalid/v1";\n'
            "fn authorize_request() {}"
        )
        self.assertIn("authorization", MODULE.detect_signals(url_then_live), "S1/C1+C2")
        self.assertNotIn(
            "authorization",
            MODULE.detect_signals("// authorize_request\nconst LIVE: usize = 1;"),
            "S2/C3 line comment",
        )
        self.assertNotIn(
            "authorization",
            MODULE.detect_signals(
                "/* outer /* nested authorize_request */ comment */\n"
                "const LIVE: usize = 1;"
            ),
            "S2/C3 block comment",
        )
        self.assertNotIn(
            "authorization",
            MODULE.detect_signals("#[cfg(test)] fn authorize_fixture() {}"),
            "S2/C3 test item",
        )
        self.assertIn(
            "authorization",
            MODULE.detect_signals(
                '#[cfg(any(test, feature = "support"))] fn authorize_support() {}'
            ),
            "S1/C4 feature alternative",
        )

    def test_exact_mechanisms_restore_safety_surfaces_without_dto_noise(self) -> None:
        # Cause/effect decision table: C1 a production module contains every
        # marker in one reviewed mechanism-level conjunction below; C2 it has
        # only a DTO/name fragment or a generic spawn; C3 the complete mechanism
        # exists only under cfg(test). Effects: E1 C1 enters its exact safety
        # category; E2 C2/C3 stay outside the denominator. The strict repository
        # gate separately binds these synthetic rules to every classified path.
        #
        # | Rule | complete conjunction | production | Effect |
        # |---|---|---|---|
        # | R1 | yes | yes | E1 exact category |
        # | R2 | no | yes | E2 no signal |
        # | R3 | yes | no, cfg(test) only | E2 no signal |
        mechanisms = [
            (
                "authorization",
                "fn validate_credential_file_ownership() { let _ = Role::Worker; }",
            ),
            ("authorization", "fn check_entitlement() {}"),
            ("authorization", "verify_current_worker_identity(directory, context, identity, now, false).await;"),
            (
                "authorization",
                "struct WorkSessionAccess; struct WorkspaceScope; "
                "fn has_work_session_access() { WorkspaceScope::non_empty(); }",
            ),
            (
                "authorization",
                "let _ = NetworkPolicy::Allowlist; "
                "let _ = EnvironmentNetworking::Limited;",
            ),
            (
                "authorization",
                "let _ = Sha256::new(); digest.update(call_id.as_bytes()); "
                "write_workspace_file();",
            ),
            (
                "authorization",
                "fn safe_root() {} let _ = Component::ParentDir;",
            ),
            (
                "authorization",
                "let _ = K8S_DNS_LABEL_MAX_LEN; "
                "let _ = blake3::hash(scope.as_bytes());",
            ),
            (
                "authorization",
                "let _ = SandboxControlBindingRequest::New; "
                "spec.automount_service_account_token = Some(false);",
            ),
            (
                "state_machine",
                "fn archive_session() { let _ = SessionDisposition::Active; }",
            ),
            (
                "state_machine",
                "let _ = TaskStatus::Working; let _ = ToolTaskPoll::Pending;",
            ),
            (
                "state_machine",
                "enum ResultMatcher { Any } fn result_matches() {}",
            ),
            (
                "state_machine",
                "session_slots.update(|slot| slot.environment_owner.begin_preparing());",
            ),
            (
                "concurrency",
                "let _: watch::Receiver<Option<JsonRpcNotifier>>; "
                "async fn published_notifier() {}",
            ),
            ("concurrency", "controller.lock().await;"),
            (
                "durability",
                "let expected_prior_commit = prior; verify_receipt();",
            ),
            (
                "durability",
                "let receipt_fingerprint = canonical_receipt(); "
                "session_cleanup_completion_admitted();",
            ),
            (
                "durability",
                "enum PublishedMemoryStream { Compact } "
                "fn selected_memory_store_bundle() {}",
            ),
            (
                "durability",
                "let _: ResourceReferenceIndex; add_reference(); remove_reference();",
            ),
            (
                "durability",
                "let _ = LiveCommand::Cancel; store.cancel(&run_id); "
                "runtime.tick_run(&run_id, clock);",
            ),
            (
                "secret_boundary",
                "struct RedactedString; fn expose_secret() {}",
            ),
            (
                "secret_boundary",
                "fn http_basic_material() -> StructuredCredentialMaterial { "
                "RedactedString::new() }",
            ),
            (
                "secret_boundary",
                "SealedAeadSecretStore::over(key, blobs);",
            ),
            (
                "secret_boundary",
                "let _: CredentialArtifactCodec; material.expose_secret();",
            ),
            (
                "secret_boundary",
                "fn classify_secret_store_error() { "
                "let _ = CredentialMaterialError::Invalid; }",
            ),
            (
                "secret_boundary",
                "fn project_work_with_secret() { secret.expose_secret(); }",
            ),
        ]
        for expected, source in mechanisms:
            self.assertIn(expected, MODULE.detect_signals(source), source)

        for source in [
            "struct CredentialArtifactCodec;",
            "use worker_security::verify_current_worker_identity;",
            "fn project_work_with_secret() {}",
            "let _: watch::Receiver<Option<JsonRpcNotifier>>;",
            "async fn published_notifier() {}",
            'const EXPECTED_PRIOR_COMMIT: &str = "expected_prior_commit";',
            "tokio::spawn(async {});",
        ]:
            self.assertEqual(MODULE.detect_signals(source), [], f"R2: {source}")

        self.assertEqual(
            MODULE.detect_signals(
                "#[cfg(test)] fn fixture() { "
                "let _ = Sha256::new(); digest.update(call_id.as_bytes()); "
                "write_workspace_file(); }"
            ),
            [],
            "R3",
        )

    def test_worker_identity_admission_survives_temporal_variant_renaming(self) -> None:
        # W1 actual production identity check remains authorization, independently
        # of the policy enum's spelling. The old scan accidentally depended on
        # TerminalGeneration + rejection_status to detect a state machine;
        # renaming the policy to SessionRoot hid an unchanged safety decision.
        # W2 a comment, import or cfg(test)-only call grants no production signal.
        # The canonical scanner/projection owns both rules; no path allowlist or
        # second classifier is introduced. This remains a literal-source inventory,
        # not compiler-level resolution of arbitrary aliases or proof coverage.
        source = (MODULE.ROOT / "crates/resources/awaken-resource-worker-http/src/worker_authority.rs").read_text()
        for variant in ("TerminalGeneration", "SessionRoot", "RootTruth"):
            self.assertIn("authorization", MODULE.detect_signals(source.replace("SessionRoot", variant)), variant)
        for source in (
            "// verify_current_worker_identity(directory, context, identity, now, false);",
            "use worker_security::verify_current_worker_identity;",
            "#[cfg(test)] fn fixture() { verify_current_worker_identity(directory, context, identity, now, false); }",
        ):
            self.assertEqual(MODULE.detect_signals(source), [], "W2")


if __name__ == "__main__":
    unittest.main()
