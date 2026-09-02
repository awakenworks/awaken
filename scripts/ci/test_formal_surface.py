#!/usr/bin/env python3
"""Focused tests for the formal production-surface inventory boundary."""

from __future__ import annotations

import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).with_name("check_formal_surface.py")
SPEC = importlib.util.spec_from_file_location("formal_surface", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FormalSurfaceProductionSourceTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
