#!/usr/bin/env python3
"""Focused tests for the formal production-surface inventory boundary."""

from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import subprocess
from collections.abc import Iterator
import pathlib
import tempfile
import unittest
import _strict_unittest as strict_unittest
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
            relative = "crates/x/src/policy.rs"
            (root / relative).parent.mkdir(parents=True)
            first = {"path": relative, "requirement_id": "domain", "reason": "owner"}
            with mock.patch.multiple(MODULE, ROOT=root):
                for second_owner in [None, "domain", "protocol", None]:
                    head = "// @formal surface requirement=domain\n"
                    if second_owner is not None:
                        head += f"// @formal surface requirement={second_owner}\n"
                    (root / relative).write_text(head + "// @formal reason: owner\nfn decide() {}")
                    if second_owner is None:
                        self.assertEqual(MODULE.load_classifications(set(), {"domain", "protocol"}),
                                         {relative: first}, "R1")
                    else:
                        message = io.StringIO()
                        with contextlib.redirect_stderr(message), self.assertRaises(SystemExit) as error:
                            MODULE.load_classifications(set(), {"domain", "protocol"})
                        self.assertEqual(error.exception.code, 1, "R2/R3")
                        self.assertIn(f"duplicate surface classification {relative}", message.getvalue())

    def test_a_source_classifies_itself_once_with_a_reason(self) -> None:
        # Causes: S1 requirement=, S2 obligation= lines, S3 exclude= with owners;
        # S4 a reason without a surface; S5 a surface without a reason; S6 an
        # owner on a non-exclusion; S7 a malformed kind; S8 two kinds at once.
        # Effects: S1-S3 one classification whose reason joins its lines;
        # S4-S8 refusal naming the file and line.
        headers = MODULE.formal_anchors.headers
        cases = [
            ("S1", ["surface requirement=a.b", "reason: one", "reason: two"], ("a.b", (), None, "one two", ())),
            ("S2", ["surface obligation=x.y", "surface obligation=x.z", "reason: r"], (None, ("x.y", "x.z"), None, "r", ())),
            ("S3", ["surface exclude=wire_dto", "owner crates/o.rs", "reason: r"], (None, (), "wire_dto", "r", ("crates/o.rs",))),
            ("S4", ["reason: r"], "needs a `// @formal surface` line"),
            ("S5", ["surface requirement=a.b"], "needs a `// @formal reason:` line"),
            ("S6", ["surface requirement=a.b", "owner crates/o.rs", "reason: r"], "belong to an exclude= surface only"),
            ("S7", ["surface proof=a.b", "reason: r"], "expected `// @formal surface <requirement|obligation|exclude>=<value>`"),
            ("S8", ["surface requirement=a.b", "surface exclude=wire_dto", "reason: r"], "duplicate surface classification"),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source = root / "crates/x/src/lib.rs"
            source.parent.mkdir(parents=True)
            for label, lines, expected in cases:
                with self.subTest(label):
                    source.write_text("".join(f"// @formal {line}\n" for line in lines) + "fn f() {}\n")
                    found, errors = MODULE.formal_anchors.surfaces(headers(root)[0])
                    if isinstance(expected, str):
                        self.assertEqual(found, {})
                        self.assertTrue(errors and errors[0].startswith("crates/x/src/lib.rs:1: "), errors)
                        self.assertIn(expected, errors[0])
                    else:
                        self.assertEqual(errors, [])
                        surface = found["crates/x/src/lib.rs"]
                        self.assertEqual((surface.requirement, surface.obligations, surface.exclude,
                                          surface.reason, surface.owners), expected)

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
            (root / policy).write_text("// @formal evidence example.policy\nfn authorize() {}")
            MODULE.formal_ledger.write(root, {"obligations": [{
                "id": "example.policy", "status": "model_linked", "evidence": [policy],
            }]})
            features = root / "features.json"
            features.write_text('{"features":[]}')
            source = "// @formal evidence example.policy\nfn authorize() {}"
            with (
                mock.patch.multiple(
                    MODULE, ROOT=root, FEATURES=features, PRODUCTION_ROOTS=[root / "crates"],
                ),
                mock.patch.object(MODULE._crate_boundary_workspace, "iter_crate_manifests", return_value=[manifest]),
                mock.patch("sys.argv", ["check_formal_surface.py", "--require-complete"]),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                MODULE.main()
                (root / policy).write_text(source.replace("\nfn", "\n// @formal surface obligation=example.policy\n"
                                                           "// @formal reason: redundant owner\nfn"))
                message = io.StringIO()
                with contextlib.redirect_stderr(message), self.assertRaises(SystemExit) as error:
                    MODULE.main()
                self.assertEqual(error.exception.code, 1, "L4")
                self.assertIn("classifications duplicate an exclusion or source-ledger link", message.getvalue(), "L4")
                (root / policy).write_text(source)
                MODULE.main()
                (crate / "src/lib.rs").write_text("")
                with self.assertRaises(SystemExit) as error:
                    MODULE.main()
                self.assertEqual(error.exception.code, 1, "L2")
                (root / policy).write_text(source.replace("\nfn", "\n// @formal surface exclude=test_only\n"
                                                           "// @formal reason: invalid camouflage\nfn"))
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
                "installed_sessions.update(|slot| slot.environment_owner.begin_preparing());",
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


class FormalSurfaceStagedSubjectTests(unittest.TestCase):
    POLICY = "crates/runtime/example/src/policy.rs"
    CLASSIFIED = (
        "// @formal surface requirement=fixture.policy\n"
        "// @formal reason: original policy\nfn authorize_request() {}\n"
    )

    def git(self, root: pathlib.Path, *arguments: str) -> str:
        # Only this disposable repository has an empty hooks directory. The
        # product's normal commit hooks and shared repository are never changed.
        return subprocess.check_output(
            ["git", "-c", "core.hooksPath=" + str(root / "empty-hooks"),
             "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
             "-C", str(root), *arguments],
            env=MODULE.formal_sources.detached_git_environment(), text=True,
        )

    @contextlib.contextmanager
    def fixture(self, *, repository: bool = True) -> Iterator[pathlib.Path]:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            features = root / "formal/features.json"
            features.parent.mkdir()
            features.write_text(json.dumps({"features": [
                {"id": "fixture", "requirements": [{"id": "policy"}]},
            ]}))
            (root / self.POLICY).parent.mkdir(parents=True)
            if repository:
                self.git(root, "init", "--quiet")
                self.git(root, "add", "formal/features.json")
            with mock.patch.multiple(MODULE, ROOT=root, FEATURES=features,
                                     PRODUCTION_ROOTS=[root / "crates"]), \
                    mock.patch.object(MODULE, "ledger_obligations", return_value=[]):
                yield root

    def invoke(self, paths: list[str], *, cli: bool = False) -> tuple[int, str, str]:
        output, error = io.StringIO(), io.StringIO()
        status = 0
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(error):
            try:
                if cli:
                    with mock.patch("sys.argv", ["check_formal_surface.py", "--staged", *paths]):
                        MODULE.main()
                else:
                    MODULE.check_staged(paths)
            except SystemExit as stopped:
                status = stopped.code
        return status, output.getvalue(), error.getvalue()

    def test_bare_and_explicit_modes_scan_the_index_not_a_benign_working_copy(self) -> None:
        # Causes C1 bare/explicit selection; C2 an unsafe staged source; C3 a
        # benign unstaged replacement. R1/R2=C1+C2+C3 -> reject that indexed
        # path. Neither an empty argv nor the working copy may earn false green.
        for paths in [[], [self.POLICY]]:
            with self.subTest(paths=paths), self.fixture() as root:
                policy = root / self.POLICY
                policy.write_text("fn authorize_request() {}\n")
                self.git(root, "add", self.POLICY)
                policy.write_text("fn harmless_working_copy() {}\n")
                status, _, error = self.invoke(paths, cli=not paths)
                self.assertEqual(status, 1)
                self.assertIn(self.POLICY, error)
                self.assertIn("unclassified", error)

    def test_classification_requirements_and_source_share_the_same_index(self) -> None:
        # R3 staged classified source + staged requirement, but both working
        # copies disagree -> accept the actual commit subject, exactly one
        # candidate. No unstaged requirement or source substitutes for the index.
        with self.fixture() as root:
            (root / self.POLICY).write_text(self.CLASSIFIED)
            self.git(root, "add", self.POLICY)
            (root / self.POLICY).write_text("fn authorize_unstaged() {}\n")
            MODULE.FEATURES.write_text('{"features": []}')
            status, output, error = self.invoke([self.POLICY])
            self.assertEqual((status, error), (0, ""))
            self.assertIn("1 production Rust candidate(s)", output)
            self.assertIn("source=index", output)

    def test_exclusion_owner_presence_is_an_index_fact(self) -> None:
        # R4 owner staged then missing on disk -> still present in commit.
        # R5 owner only on disk -> reject the staged exclusion; it names no
        # committed implementation. A working file cannot authorize an exclusion.
        owner = "crates/runtime/example/src/owner.rs"
        for staged_owner in [False, True]:
            with self.subTest(staged_owner=staged_owner), self.fixture() as root:
                (root / self.POLICY).write_text(
                    "// @formal surface exclude=application_adapter\n"
                    f"// @formal owner {owner}\n// @formal reason: original owner\n"
                    "fn authorize_request() {}\n"
                )
                (root / owner).write_text("fn original_owner() {}\n")
                self.git(root, "add", self.POLICY)
                if staged_owner:
                    self.git(root, "add", owner)
                    (root / owner).unlink()
                status, _, error = self.invoke([self.POLICY])
                self.assertEqual(status, 0 if staged_owner else 1)
                if not staged_owner:
                    self.assertIn(owner, error)
                    self.assertIn("missing owner", error)

    def test_staged_source_cannot_hide_by_disappearing_from_disk(self) -> None:
        # R6 staged unsafe source + missing working file -> reject its indexed
        # bytes. Disk absence is not deletion from the selected Git subject.
        with self.fixture() as root:
            (root / self.POLICY).write_text("fn authorize_request() {}\n")
            self.git(root, "add", self.POLICY)
            (root / self.POLICY).unlink()
            status, _, error = self.invoke([])
            self.assertEqual(status, 1)
            self.assertIn(self.POLICY, error)

    def test_scope_counts_ignore_docs_and_unstaged_sources_but_preserve_deletions(self) -> None:
        # C4 docs-only/unstaged-only/deleted source. R7/R8 -> zero real staged
        # Rust candidates, honestly reported. R9 explicit staged deletion ->
        # skip the absent blob; R10 unknown/untracked explicit Rust -> refuse,
        # never claim a source was checked merely because its path was supplied.
        with self.fixture() as root:
            (root / self.POLICY).write_text("fn original() {}\n")
            self.git(root, "add", self.POLICY)
            self.git(root, "commit", "--quiet", "-m", "fixture baseline")
            (root / self.POLICY).write_text("fn authorize_unstaged() {}\n")
            for docs in [False, True]:
                if docs:
                    (root / "README.md").write_text("Only documentation.\n")
                    self.git(root, "add", "README.md")
                status, output, error = self.invoke([])
                self.assertEqual((status, error), (0, ""))
                self.assertIn("0 production Rust candidate(s)", output)
            self.git(root, "rm", "--force", "--quiet", self.POLICY)
            status, output, error = self.invoke([self.POLICY])
            self.assertEqual((status, error), (0, ""))
            self.assertIn("0 production Rust candidate(s)", output)
            missing = "crates/runtime/example/src/untracked.rs"
            (root / missing).parent.mkdir(parents=True, exist_ok=True)
            (root / missing).write_text("fn authorize_untracked() {}\n")
            status, _, error = self.invoke([missing])
            self.assertEqual(status, 1)
            self.assertIn("absent from the index", error)

    def test_renamed_paths_use_the_canonical_nul_framed_git_reader(self) -> None:
        # R11 a rename target contains a newline and unsafe code -> inspect the
        # one complete path, not display-quoted text or two partial path names.
        with self.fixture() as root:
            (root / self.POLICY).write_text("fn original() {}\n")
            self.git(root, "add", self.POLICY)
            self.git(root, "commit", "--quiet", "-m", "fixture baseline")
            renamed = "crates/runtime/example/src/odd\nname.rs"
            self.git(root, "mv", self.POLICY, renamed)
            (root / renamed).write_text("fn authorize_renamed() {}\n")
            self.git(root, "add", renamed)
            status, _, error = self.invoke([])
            self.assertEqual(status, 1)
            self.assertIn(renamed, error)

    def test_failed_index_selection_or_blob_io_never_becomes_empty_success(self) -> None:
        # R12 failed selection and R13 failed selected-blob IO both refuse;
        # neither may fall back to a benign disk file or a zero-source result.
        with self.fixture() as root:
            (root / self.POLICY).write_text(self.CLASSIFIED)
            self.git(root, "add", self.POLICY)
            with mock.patch.object(MODULE.formal_sources, "git_paths",
                                   side_effect=OSError("index unavailable")):
                status, _, error = self.invoke([])
            self.assertEqual(status, 1)
            self.assertIn("index unavailable", error)
            reader = MODULE.formal_sources.file_text

            def fail_selected(source_root, relative, staged):
                if relative == self.POLICY:
                    raise MODULE.formal_sources.SourceError("selected blob unavailable")
                return reader(source_root, relative, staged)

            with mock.patch.object(MODULE.formal_sources, "file_text", side_effect=fail_selected):
                status, _, error = self.invoke([self.POLICY])
            self.assertEqual(status, 1)
            self.assertIn("selected blob unavailable", error)

    def test_outside_paths_and_unreadable_repository_markers_are_refusals(self) -> None:
        # R16 an explicit foreign Rust path -> refuse the wrong source root.
        # R17 discovery fails while .git still marks this as a repository ->
        # refuse, never reinterpret a damaged repository as a disk fixture.
        with self.fixture() as root, tempfile.TemporaryDirectory() as foreign:
            path = pathlib.Path(foreign) / "policy.rs"
            path.write_text("fn authorize_foreign() {}\n")
            status, _, error = self.invoke([str(path)])
            self.assertEqual(status, 1)
            self.assertIn("outside the source root", error)
            (root / self.POLICY).write_text(self.CLASSIFIED)
            self.git(root, "add", self.POLICY)
            with mock.patch.object(MODULE.formal_sources, "repository_environment", return_value=None):
                status, _, error = self.invoke([self.POLICY])
            self.assertEqual(status, 1)
            self.assertIn("repository index cannot be selected", error)

    def test_disk_fixture_requires_explicit_selection(self) -> None:
        # R14 explicit non-repository fixture -> existing disk predicate; R15
        # bare --staged outside a repository -> refusal, no fabricated index.
        with self.fixture(repository=False) as root:
            (root / self.POLICY).write_text(self.CLASSIFIED)
            status, output, error = self.invoke([self.POLICY])
            self.assertEqual((status, error), (0, ""))
            self.assertIn("source=explicit disk fixture", output)
            status, _, error = self.invoke([], cli=True)
            self.assertEqual(status, 1)
            self.assertIn("Cannot read staged formal source", error)


if __name__ == "__main__":
    strict_unittest.main()
