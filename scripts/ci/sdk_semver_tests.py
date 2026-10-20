"""Controlled evidence cases loaded by the original SDK checker's self-test."""
import json
import copy
import contextlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import unittest
from unittest import mock

import _sdk_semver_evidence as evidence


def major_source_files(current, declared=None):
    """Private source data reused by the action and outer controlled fixtures."""
    declared = {name: (declared or {}).get(name, {})
                for name in evidence.consumer_package_names()}
    files = {"Cargo.toml": '[workspace]\nmembers=["crates/*"]\n[workspace.package]\nversion="1.0.0"\n', "Cargo.lock": 'version=4\n'}
    for name in evidence.consumer_package_names():
        versions = evidence.COHORT.get(name, ("1.0.0", "1.0.0"))
        version = versions[int(current)]
        authored = '\nversion.workspace=true' if version == "1.0.0" else '\nversion=' + json.dumps(version)
        text = ('[package]\nname=' + json.dumps(name) + authored
                + '\n[package.metadata.awaken]\nproduct="sdk"\napi_snapshot=true\n')
        if declared[name]:
            text += '[features]\n' + ''.join(key + '=' + json.dumps(value) + '\n' for key, value in declared[name].items())
        if name == "awaken-runtime-contract":
            text += '[dependencies]\n' + ''.join(owner + '={path="../' + owner + '"}\n'
                                                   for owner in evidence.WITNESS_OWNERS)
        files['crates/' + name + '/Cargo.toml'] = text
    return files


def major_metadata_fixture(sdk_root, *, current, feature, consumer=None, declared=None):
    """Cargo protocol fixture data; this function starts no process."""
    declared = {name: (declared or {}).get(name, {})
                for name in evidence.consumer_package_names()}
    packages, nodes = [], []
    identities = {name: ('current:' if current else 'baseline:') + name for name in evidence.consumer_package_names()}
    for name in evidence.consumer_package_names():
        versions = evidence.COHORT.get(name, ("1.0.0", "1.0.0"))
        packages.append({'id': identities[name], 'name': name, 'version': versions[int(current)],
            'manifest_path': str(sdk_root / 'crates' / name / 'Cargo.toml'), 'source': None,
            'features': declared[name], 'dependencies': [], 'metadata': {'awaken': {'product': 'sdk', 'api_snapshot': True}}})
        nodes.append({'id': identities[name], 'features': sorted(declared[name]) if feature == 'all' else
                      (['default'] if 'default' in declared[name] else []),
                      'deps': [{'name': 'genai', 'pkg': 'genai-current' if current else 'genai-old'}]
                      if name == 'awaken-provider-genai' else []})
        if name == "awaken-runtime-contract":
            nodes[-1]['deps'] = [
                {'name': owner.replace('-', '_'), 'pkg': identities[owner],
                 'dep_kinds': [{'kind': None, 'target': None}]} for owner in evidence.WITNESS_OWNERS]
            packages[-1]['dependencies'] = [{'name': owner, 'kind': None, 'rename': None}
                                             for owner in evidence.WITNESS_OWNERS]
    for selected_current in ([False, True] if consumer is not None and current else [current]):
        identity = 'genai-current' if selected_current else 'genai-old'
        packages.append({'id': identity, 'name': 'genai', **evidence.GENAI_SOURCES[selected_current],
                         'manifest_path': str(sdk_root / ('external-' + identity) / 'Cargo.toml'), 'features': {}})
        nodes.append({'id': identity, 'features': ['default', 'rustls-tls', *(['system-proxy'] if selected_current else [])], 'deps': []})
    roots = list(identities.values())
    workspace = sdk_root
    if consumer is not None:
        roots, workspace = ['consumer'], consumer
        packages.append({'id': 'consumer', 'name': 'sdk-major-observation', 'version': '0.0.0',
                         'manifest_path': str(consumer / 'Cargo.toml'), 'source': None, 'features': {}})
        nodes.append({'id': 'consumer', 'features': [], 'deps': [
            *[{'name': name.replace('-', '_'), 'pkg': identity,
               'dep_kinds': [{'kind': None, 'target': None}]} for name, identity in identities.items()],
            {'name': 'genai_old', 'pkg': 'genai-old'},
            *([{'name': 'genai', 'pkg': 'genai-current'}] if current else [])]})
    return {'workspace_root': str(workspace), 'workspace_members': roots, 'packages': packages,
            'resolve': {'nodes': nodes}}


class SdkSemverEvidenceTests(unittest.TestCase):
    @staticmethod
    def report(name, *, checks=222, failures=(), major=False):
        failed = len({item[0] for item in failures})
        old, new = evidence.COHORT.get(name, ("1.0.0", "1.0.0"))
        change = "major change" if major else "assume patch change"
        value = (f"Checking {name} v{old} -> v{new} ({change})\n"
                 f"Checked [0.1s] {checks} checks: {checks-failed} pass")
        if failed:
            value += f", {failed} fail, 0 warn"
        value += f", {252-checks} skip\n"
        for lint in sorted({item[0] for item in failures}):
            value += f"--- failure {lint}: fixture finding ---\nFailed in:\n"
            for item in failures:
                if item[0] == lint:
                    if lint == "inherent_method_missing":
                        value += f"  {item[2]}, previously in file baseline.rs:1\n"
                    elif lint == "enum_variant_added":
                        value += f"  variant {item[2]} in current.rs:1\n"
                    else:
                        value += f"  {item[1]} {item[2]}, previously in file baseline.rs:1\n"
        summary = (f"semver requires new major version: {failed} major and 0 minor checks failed"
                   if failed else "no semver update required")
        return value + f"Summary {summary}\nFinished [0.1s] {name}\n"

    def fixture(self):
        # These are controlled parser inputs, never the production allow set.
        known = {name: set() for name in evidence.COHORT}
        known["awaken-ext-builtin-tools"] = {
            ("function_missing", "function", "fixture::removed"),
            ("struct_missing", "struct", "fixture::first"),
            ("struct_missing", "struct", "fixture::second"),
        }
        output = "".join(self.report(name, failures=known[name]) for name in sorted(known))
        return known, output

    def test_nonempty_audit_counts_lints_separately_from_affected_items(self):
        # A1 C1=two failed queries, C2=three affected items, C3=complete reports
        # -> retain both counts and original tool1; no item/query conflation.
        known, output = self.fixture()
        rows = evidence.validate_audit(output, 1, approved=known)
        self.assertEqual(rows["awaken-ext-builtin-tools"]["failed"], 2)
        self.assertEqual(len(rows["awaken-ext-builtin-tools"]["findings"]), 3)
        self.assertEqual(rows["awaken-provider-genai"]["checks"], 222)

    def test_pinned_variant_and_inherent_method_rows_are_complete_observations(self):
        # B1-B5 cause/effect table, matching the actual pinned0.47 reports:
        # typed removed item / unprefixed method / current variant -> retain
        # each exact item; multiple items under one lint -> one failed query.
        # Wrong lint/shape, duplicate item/block, or an unknown extra row in
        # an otherwise complete block -> refusal. Parsing grants no approval;
        # version policy and original tool status remain separate decisions.
        findings = {
            ("inherent_method_missing", "method", "Runtime::session"),
            ("inherent_method_missing", "method", "Runtime::with_max_consecutive_inference_failures"),
            ("enum_variant_added", "variant", "Error:Admission"),
        }
        output = self.report("awaken-runtime", failures=findings)
        row = evidence.observations(output, ["awaken-runtime"], strict=True)["awaken-runtime"]
        self.assertEqual(row["failed"], 2)
        self.assertEqual(set(row["findings"]), findings)
        variant = "  variant Error:Admission in current.rs:1\n"
        method = "  Runtime::session, previously in file baseline.rs:1\n"
        for changed in (
            output.replace(variant, variant.replace(" in ", " at ")),
            output.replace(method, "  method Runtime::session, previously in file baseline.rs:1\n"),
            output.replace(method, method * 2),
            output.replace(variant, variant + "  variant hidden with unsupported syntax\n"),
            output.replace("--- failure enum_variant_added:", "--- failure function_missing:"),
            output.replace("Failed in:\n", "Failed in:\nFailed in:\n", 1),
        ):
            with self.subTest(report=changed), self.assertRaises(ValueError):
                evidence.observations(changed, ["awaken-runtime"], strict=True)

    def test_missing_duplicate_unknown_or_empty_audit_refuses(self):
        # A2 complete known set + checked + Finished + declared versions is
        # required conjunctively. Each single missing/extra/duplicate condition
        # refuses; resource/tool failure is not a semantic report.
        known, output = self.fixture()
        invalid = (
            output.replace("fixture::removed", "fixture::unreviewed"),
            output.replace("Finished [0.1s] awaken-provider-genai", ""),
            output + output,
            output.replace("v2.0.0", "v3.0.0", 1),
            output.replace("222 checks: 222 pass, 30 skip", "0 checks: 0 pass, 252 skip"),
            output.replace("  function fixture::removed, previously in file baseline.rs:1\n", ""),
            output.replace("  function fixture::removed, previously in file baseline.rs:1\n",
                           "  function fixture::removed, previously in file baseline.rs:1\n" * 2),
        )
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                evidence.validate_audit(value, 1, approved=known)
        for raw in (0, 75, 101, 124, 143):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                evidence.validate_audit(output, raw, approved=known)
        # An explicitly unestablished policy remains refusal after production
        # data have been reviewed; do not accidentally test a fixture mismatch.
        with self.assertRaises(ValueError):
            evidence.validate_audit(output, 1, approved=None)

    def test_known_findings_and_finished_do_not_absorb_an_abnormal_exit_one_tail(self):
        # A6 exact known findings + Finished is insufficient: the pinned Summary
        # must account for queries (not affected items), and fatal/compiler/panic
        # output cannot be mistaken for the expected semantic exit1. Ordinary
        # Description or warning text is not a fatal prefix.
        known, output = self.fixture()
        for suffix in ("error: internal/tool failure", "error[E0308]: compiler failure",
                       "Error: unable to finish report", "thread 'main' panicked at source.rs:1",
                       "fatal: unknown source", "Caused by: transport failure"):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                evidence.validate_audit(output + suffix + "\n", 1, approved=known)
        for changed in (output.replace("2 major and 0 minor", "3 major and 0 minor"),
                        output.replace("Summary semver requires new major version: 2 major and 0 minor checks failed\n", ""),
                        output.replace("Summary no semver update required", "Summary incomplete")):
            with self.subTest(summary=changed), self.assertRaises(ValueError):
                evidence.validate_audit(changed, 1, approved=known)
        evidence.validate_audit(output + "warning: a normal advisory\n", 1, approved=known)

    def test_major_zero_requires_corresponding_nonempty_audit(self):
        # A3 unchanged nonmajor zero -> refusal; A4 declared major zero without
        # its reviewed nonempty package audit -> refusal; A5 exact observed
        # major + that audit -> accepted observation, never a zero-check pass.
        known, audit_output = self.fixture()
        audited = evidence.validate_audit(audit_output, 1, approved=known)
        names = sorted(known)
        output = "".join(self.report(name, checks=0, major=True) for name in names)
        for proof in (None, {}, {names[0]: audited[names[0]]}):
            with self.subTest(proof=proof), self.assertRaises(ValueError):
                evidence.validate_standard(output, names, proof)
        self.assertEqual(set(evidence.validate_standard(output, names, audited)), set(names))
        nonmajor = self.report("other-sdk", checks=0)
        with self.assertRaises(ValueError):
            evidence.validate_standard(nonmajor, ["other-sdk"], {"other-sdk": audited[names[0]]})

    def test_runtime_release_requires_its_own_nonempty_reviewed_audit(self):
        # R6/R7 C/E: all declared reports + exact approvals + nonempty queries
        # -> observe; for each Runtime/Contract package, missing approval,
        # unknown finding or zero queries -> refuse. Existing Runtime rows stay
        # intact. Empty sets here are controlled fixtures, never production APP
        # or actual SemVer observations; original raw1 still belongs to Builtin.
        known, output = self.fixture()
        for name, item in (("awaken-runtime", "fixture::runtime_unreviewed"),
                           ("awaken-runtime-contract", "fixture::runtime_contract_unreviewed")):
            with self.subTest(package=name):
                partial = {package: findings for package, findings in known.items() if package != name}
                with self.assertRaises(ValueError):
                    evidence.validate_audit(output, 1, approved=partial)
                runtime = self.report(name)
                changed = output.replace(runtime, self.report(name, failures={
                    ("function_missing", "function", item),
                }))
                with self.assertRaises(ValueError):
                    evidence.validate_audit(changed, 1, approved=known)
                zero = output.replace(runtime, self.report(name, checks=0))
                with self.assertRaises(ValueError):
                    evidence.validate_audit(zero, 1, approved=known)
                observed = evidence.validate_audit(output, 1, approved=known)
                self.assertGreater(observed[name]["checks"], 0)

        # R8-R10 exercise the production default policy, independently of the
        # controlled empty Runtime fixtures above. The two actual reviewed
        # report sets are necessary; dropping a reviewed item or adding an
        # unreviewed one refuses. This checks policy application only, never
        # substitutes for original compiler/report/source-profile receipts.
        reviewed = {
            "awaken-runtime": {
                ("inherent_method_missing", "method", "Runtime::session"),
                ("inherent_method_missing", "method", "Runtime::with_max_consecutive_inference_failures"),
                ("struct_missing", "struct", "awaken_runtime::EmbeddedSession"),
            },
            "awaken-runtime-contract": {
                ("enum_variant_added", "variant", "Error:Admission"),
                ("module_missing", "mod", "awaken_runtime_contract::credential"),
            },
        }
        reports = {
            name: self.report(name, failures=reviewed.get(name, evidence.APPROVED_FINDINGS[name]))
            for name in evidence.COHORT
        }
        production_output = "".join(reports.values())
        self.assertEqual(set(evidence.validate_audit(production_output, 1)), set(evidence.COHORT))
        for name, findings in reviewed.items():
            for changed in [*(findings - {item} for item in findings),
                            findings | {("function_missing", "function", "fixture::unreviewed")}]:
                with self.subTest(package=name, findings=changed), self.assertRaises(ValueError):
                    evidence.validate_audit(production_output.replace(reports[name], self.report(name, failures=changed)), 1)

    @staticmethod
    def compiler_output(code=None, *, file="cases/old_enum.rs", line=2, finished=True):
        rows = []
        if code:
            rows.append({"reason": "compiler-message", "message": {
                "level": "error", "code": {"code": code},
                "spans": [{"file_name": file, "line_start": line, "is_primary": True}]}})
        rows.append({"reason": "build-finished", "success": finished})
        return "\n".join(json.dumps(row) for row in rows)

    def test_compiler_counterexample_requires_code_source_span_and_positive_control(self):
        # W1 baseline/migrated positive compiler0+build-finished true -> pass;
        # W2 negative101+exact E0004/0308/0425+single original primary span ->
        # counterexample data. W3 any wrong code/span/raw/empty/duplicate report
        # or positive failure -> refusal, never a generic acceptance of101.
        source = "fn main() {\n    match original { }\n}\n"
        good = self.compiler_output("E0004", finished=False)
        self.assertEqual(len(evidence.validate_compiler_case(good, 101, source,
                         "cases/old_enum.rs", "E0004", "match original")), 1)
        for output, raw in (("", 101), (good, 0), (good, 75),
                            (good.replace("E0004", "E0308"), 101),
                            (good.replace("cases/old_enum", "dependency/lib"), 101),
                            (good.replace('"line_start": 2', '"line_start": 3'), 101),
                            (good + "\n" + good, 101)):
            with self.subTest(raw=raw, output=output), self.assertRaises(ValueError):
                evidence.validate_compiler_case(output, raw, source, "cases/old_enum.rs", "E0004", "match original")
        diagnostic = json.loads(good.splitlines()[0])
        diagnostic["message"]["spans"] *= 2
        duplicate_span = json.dumps(diagnostic) + "\n" + good.splitlines()[-1]
        extra_error = good.splitlines()[0] + "\n" + good
        for output in (duplicate_span, extra_error):
            with self.subTest(output=output), self.assertRaises(ValueError):
                evidence.validate_compiler_case(output, 101, source, "cases/old_enum.rs", "E0004", "match original")
        for raw in (75, 101, 124, 143):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                evidence.validate_compiler_case(self.compiler_output(), raw, source, "cases/old_enum.rs", None, "")
        self.assertEqual(evidence.validate_compiler_case(self.compiler_output(), 0, source,
                         "cases/old_enum.rs", None, ""), [])

        # W4: the four actual crate-qualified missing-type diagnostics are
        # E0425 at the original primary span. Exact raw101+E0425 -> observed
        # counterexample; the former E0412 expectation -> refuse, never accept
        # an arbitrary101. Baseline/migration controls remain required above.
        for case in ("old_runtime_type", "owner_old_message_id",
                     "owner_old_credential_root", "owner_old_tool_root"):
            code, marker = evidence.witness_cases(True)[case]
            text = evidence.WITNESS_SOURCES[case]
            line = next(i for i, value in enumerate(text.splitlines(), 1) if marker in value)
            file = "cases/" + case + ".rs"
            output = self.compiler_output("E0425", file=file, line=line, finished=False)
            with self.subTest(case=case):
                self.assertEqual(len(evidence.validate_compiler_case(
                    output, 101, text, file, code, marker)), 1)
                with self.assertRaises(ValueError):
                    evidence.validate_compiler_case(output.replace("E0425", "E0412"),
                        101, text, file, code, marker)

    def test_incomplete_witness_receipt_is_not_nine_fabricated_case_claims(self):
        # Active-major S4 red: correct nine code/span/source-hash case claims do
        # not prove execution. Empty side facts, a foreign target, no steps/logs
        # and missing manifest/lock evidence must refuse. This reproduces the
        # retained pure-validator counterexample; no Cargo/SDK is executed.
        source = {"revision": "a" * 40, "tree": "b" * 40,
                  "index_tree": "b" * 40, "working_subject": "c" * 64}
        receipt = {"source": source, "baseline_rev": "d" * 40, "feature": "all",
            "exit_code": 0, "sides": {"baseline": {}, "current": {}},
            "baseline_archive_sha256": "e" * 64, "target": "foreign-target", "steps": [], "cases": []}
        for current, side in ((False, "baseline"), (True, "current")):
            for name, (code, marker) in evidence.witness_cases(current).items():
                text = evidence.WITNESS_SOURCES[name]
                errors = []
                if code:
                    line = next(number for number, value in enumerate(text.splitlines(), 1) if marker in value)
                    errors = [json.loads(self.compiler_output(code, file="cases/" + name + ".rs",
                                                            line=line, finished=False).splitlines()[0])["message"]]
                receipt["cases"].append({"side": side, "case": name, "original_exit_code": 101 if code else 0,
                    "expected_code": code, "compiler_errors": errors,
                    "source_sha256": evidence.hashlib.sha256(text.encode()).hexdigest()})
        with self.assertRaises(ValueError):
            evidence.validate_witness_receipt(receipt, source, "d" * 40, "all")


def shared_sdk_io_tests(owner):
    """Use the running SDK owner; never import it as a second module instance."""
    class SharedSdkIOTests(unittest.TestCase):
        @staticmethod
        def facts(*, dirty=False):
            return {"revision": b"a" * 40 + b"\n", "tree": b"b" * 40 + b"\n",
                    "index_tree": b"b" * 40 + b"\n", "status": b" M tracked.txt\n" if dirty else b"",
                    "staged": b"", "working": b"changed" if dirty else b"", "untracked": b""}

        def test_stdio_adapter_keeps_split_merged_status_and_spawn_observations(self):
            # D1 split bare capture vs D2 merged qualification capture: identical
            # argv/env/cwd/check and original0/23/75/signal remain unchanged; the
            # returned CompletedProcess is the original object. D3 spawn failure
            # propagates once, with no retry or synthetic successful observation.
            command = ["cargo", "check", "--literal", "sdk"]
            arguments = {"cwd": Path("fixture"), "env": {"SDK_IO": "fixture"}, "check": False}
            for merged in (False, True):
                stdio = ({"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT}
                         if merged else {"capture_output": True})
                for raw in (0, 23, 75, -15):
                    completed = subprocess.CompletedProcess(command, raw, "original output\n", None if merged else "original diagnostic\n")
                    with self.subTest(merged=merged, raw=raw), mock.patch.object(owner.subprocess, "run", return_value=completed) as run:
                        observed = owner.capture_sdk_command(command, merged=merged, **arguments)
                        self.assertIs(observed, completed)
                        self.assertEqual(observed.returncode, raw)
                        self.assertEqual(observed.stderr, None if merged else "original diagnostic\n")
                        run.assert_called_once_with(command, text=True, **stdio, **arguments)
                with mock.patch.object(owner.subprocess, "run", side_effect=OSError("original spawn refusal")) as run:
                    with self.assertRaisesRegex(OSError, "original spawn refusal"):
                        owner.capture_sdk_command(command, merged=merged, **arguments)
                    run.assert_called_once()

        def test_metadata_json_keeps_diagnostics_separate_and_allows_the_original_successors(self):
            # D9 qualification asks for merged diagnostics, but Cargo metadata
            # owns a stdout JSON protocol. Valid JSON + normal stderr warnings
            # => the existing sequence reaches all original eight tasks and its
            # sample command. D10 bad stdout JSON ->1 before either successor;
            # D11 child75 remains75. This is controlled IO, not artifact or SDK
            # execution evidence. No prefix stripping or --quiet is involved.
            warning = "    Updating crates.io index\nwarning: patch fixture was not used\n"
            tasks = "\n".join(f"test {name} ... ok" for name in sorted(owner.EXPECTED))
            tasks += f"\ntest result: ok. {len(owner.EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
            for cargo in (["cargo"], ["cargo", "+1.96.0"], [str(Path("tools/cargo.cmd"))]):
                for bad_json, raw, expected in ((False, 0, 0), (True, 0, 1), (False, 75, 75)):
                    with self.subTest(cargo=cargo, bad_json=bad_json, raw=raw), tempfile.TemporaryDirectory() as temporary:
                        root = Path(temporary)
                        command = [*cargo, "metadata", "--format-version", "1"]
                        sequence = [command,
                            ["cargo", "test", "--test", "tasks", "--", "--test-threads=1"],
                            ["cargo", "run"]]
                        calls, completed = [], None
                        metadata = "bad-json" if bad_json else json.dumps({"workspace_root": str(root)})
                        def spawn(actual, **arguments):
                            calls.append(actual)
                            if actual == command:
                                merged = arguments.get("stderr") == subprocess.STDOUT
                                return subprocess.CompletedProcess(actual, raw,
                                    warning + metadata if merged else metadata, None if merged else warning)
                            return subprocess.CompletedProcess(actual, 0, tasks if actual[1] == "test" else "sample output\n", None)
                        def capture(actual, **arguments):
                            nonlocal completed
                            completed = owner.capture_sdk_command(actual, merged=True, **arguments)
                            return completed
                        def record(rows):
                            label = str(len(rows))
                            owner.retain_sdk_command_output(root, label, completed)
                            if rows[-1]["exit_code"]:
                                return
                            if rows[-1]["command"] == command:
                                self.assertEqual(json.loads(completed.stdout)["workspace_root"], str(root))
                            elif rows[-1]["command"][1] == "test":
                                owner.validate_task_evidence(completed.stdout)
                        with mock.patch.object(owner.subprocess, "run", side_effect=spawn), \
                                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                            status, rows = owner.crate_test.execute_commands(sequence, {}, root=root, run=capture, record=record)
                        self.assertEqual(status, expected)
                        self.assertEqual(calls, sequence if expected == 0 else [command])
                        self.assertEqual(rows[0]["exit_code"], raw)
                        self.assertEqual((root / "1.stdout.log").read_text(), metadata)
                        self.assertEqual((root / "1.stderr.log").read_text(), warning)
                        self.assertEqual((root / "1.log").read_text(), metadata + warning)

            # D12 non-metadata qualification subjects keep merged IO; bare
            # subjects keep split IO. A later literal "metadata" argument must
            # not be mistaken for Cargo's subcommand.
            for command in (["cargo", "semver-checks", "check-release"],
                            ["cargo", "+1.96.0", "package"],
                            ["cargo", "test", "metadata"], ["python3", "metadata"]):
                for merged in (False, True):
                    with self.subTest(command=command, merged=merged), mock.patch.object(owner.subprocess, "run") as run:
                        owner.capture_sdk_command(command, merged=merged)
                        self.assertEqual(run.call_args.kwargs.get("stderr"), subprocess.STDOUT if merged else None)
                        self.assertEqual(run.call_args.kwargs.get("capture_output"), None if merged else True)

        def test_shared_observation_keeps_bare_wip_and_qualification_clean_policy(self):
            # D4 the renderer only observes original facts. D5 initial WIP is
            # allowed by the ordinary consumer, while D6 package/semver require
            # clean source before tools or metadata; moving the renderer must
            # not merge those different policies. Original drift/failure rows
            # remain covered by the owning SDK suite, loaded only once.
            facts = self.facts(dirty=True)
            with mock.patch.object(owner.rust_test_plan, "working_subject", return_value=("original-subject", facts)):
                observed, raw, rendered = owner.sdk_source_observation()
                self.assertEqual(observed, "original-subject")
                self.assertIs(raw, facts)
                self.assertEqual(rendered, {"revision": "a" * 40, "tree": "b" * 40,
                    "index_tree": "b" * 40, "working_subject": "original-subject"})
            helper = owner.SdkConsumerTests()  # Reuse fixture methods, not its runnable suite.
            for qualification in ("", "package", "semver"):
                with self.subTest(qualification=qualification), tempfile.TemporaryDirectory() as temporary:
                    root = helper.repository(Path(temporary), successor=True)
                    with mock.patch.object(owner.rust_test_plan, "working_subject", return_value=("original-subject", facts)), \
                            mock.patch.object(owner, "sdk_source_observation", wraps=owner.sdk_source_observation) as observe:
                        status, calls = helper.execute(root, qualification=qualification)
                    self.assertEqual(status, 1 if qualification else 0)
                    self.assertEqual(len(calls), 0 if qualification else 2)
                    self.assertGreater(observe.call_count, 0)

        @contextlib.contextmanager
        def major_action(self, root, raw, *, cause="", compiler=False, full=False,
                         feature="default", failure_step="", drift="", external_spec=None,
                         declared=None, archive_cause=""):
            target = root / "target"
            spec = Path(external_spec) if external_spec else target / "test-timings/spec.json"
            spec.parent.mkdir(parents=True, exist_ok=True)
            supplied = json.loads(spec.read_text()) if external_spec else None
            feature = supplied["feature"] if supplied else feature
            declarations = declared or {side: {name: {} for name in evidence.COHORT} for side in ("baseline", "current")}
            if compiler:
                for name, value in major_source_files(True, declarations["current"]).items():
                    if external_spec:
                        if name.startswith("crates/"):
                            assert (root / name).read_text() == value
                        continue
                    destination = root / name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_text(value)
            facts = self.facts(dirty=raw is None)
            subject = "original-subject"
            if supplied:
                subject = supplied["source"]["working_subject"]
                facts.update({name: supplied["source"][name].encode() + b"\n"
                              for name in ("revision", "tree", "index_tree")})
            original_write = Path.write_text
            original_sequence = owner.crate_test.execute_commands
            original_cases = owner.semver_evidence.witness_cases
            state = {"ran": False, "faulted": False, "journal_writes": 0, "rows": []}

            def observe():
                dirty = cause == "source" and state["ran"]
                return subject, {**facts, "status": b" M changed\n"} if dirty else facts

            def write(path, text, *arguments, **options):
                if state["ran"] and path.name == "commands.json":
                    state["journal_writes"] += 1
                failed_path = (path.name == "commands.json" if cause == "journal"
                               else path.name.endswith(".stdout.log") if cause == "log" else False)
                if state["ran"] and failed_path and not state["faulted"]:
                    state["faulted"] = True
                    raise OSError("exact " + cause + " retention refusal")
                return original_write(path, text, *arguments, **options)

            def archive(command):
                with tarfile.open(command[command.index("--output") + 1], "w") as output:
                    for name, value in major_source_files(False, declarations["baseline"]).items():
                        item = tarfile.TarInfo(name)
                        data = value.encode()
                        item.size = len(data)
                        output.addfile(item, io.BytesIO(data))
                    if archive_cause:
                        item = tarfile.TarInfo("../outside" if archive_cause == "escape" else "external-link")
                        if archive_cause == "link":
                            item.type, item.linkname = tarfile.SYMTYPE, "../../outside"
                        output.addfile(item)

            def run(command, **arguments):
                if not compiler:
                    state["ran"] = True
                    if raw == 0:
                        archive(command)
                    return subprocess.CompletedProcess(command, raw if isinstance(raw, int) else 0,
                                                       "original archive diagnostic\n", None)
                specification = json.loads(spec.read_text())
                work = spec.parent / (feature + "-major-consumers")
                step = next(row for row in evidence.witness_steps(root, work, specification["baseline_rev"], feature, owner.SDK_TARGET)
                            if row["command"] == command)
                name = step["name"]
                chosen = failure_step or "current-old_enum"
                state["ran"] = state["ran"] or name == chosen
                status = raw if failure_step and name == failure_step else step["exit_code"]
                if command[:2] == ["git", "archive"]:
                    if status == 0:
                        archive(command)
                    return subprocess.CompletedProcess(command, status, "archive diagnostic\n", None)
                manifest = Path(command[command.index("--manifest-path") + 1])
                current = manifest.parent == root or manifest.parent.name == "current-consumer"
                side = "current" if current else "baseline"
                sdk_root = root if current else work / "baseline-source"
                if command[1] == "metadata":
                    consumer = manifest.parent if manifest.parent.name.endswith("-consumer") else None
                    if consumer is not None:
                        (consumer / "Cargo.lock").write_text("private lock fixture\n")
                    metadata = major_metadata_fixture(sdk_root, current=current, feature=feature,
                                                      consumer=consumer, declared=declarations[side])
                    return subprocess.CompletedProcess(command, status, json.dumps(metadata), "metadata diagnostic\n")
                case = command[command.index("--bin") + 1]
                code, marker = original_cases(current)[case]
                source = evidence.WITNESS_SOURCES[case]
                line = next(number for number, text in enumerate(source.splitlines(), 1) if marker in text) if code else 2
                if cause == "code" and name == chosen:
                    code = "E0412"
                output = SdkSemverEvidenceTests.compiler_output(code, file="cases/" + case + ".rs", line=line, finished=code is None)
                if not full and current and not failure_step:
                    status = raw
                if drift and name == chosen:
                    destination = (manifest.parent / "cases" / (case + ".rs") if drift == "consumer-source"
                                   else manifest.parent / "Cargo.lock" if drift == "consumer-lock"
                                   else work / "baseline-source/Cargo.toml")
                    destination.write_text("changed owned input\n")
                return subprocess.CompletedProcess(command, status, output, None)

            def sequence(*arguments, **options):
                result = original_sequence(*arguments, **options)
                state["rows"].extend(result[1])
                return result

            with mock.patch.object(owner, "ROOT", root), \
                    mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target)), \
                    mock.patch.object(owner.rust_test_plan, "working_subject", side_effect=observe), \
                    mock.patch.object(owner, "sdk_source_observation", wraps=owner.sdk_source_observation) as observation, \
                    mock.patch.object(owner, "capture_sdk_command", wraps=owner.capture_sdk_command) as capture, \
                    mock.patch.object(owner.subprocess, "run", side_effect=run) as spawn, \
                    mock.patch.object(owner.crate_test, "execute_commands", side_effect=sequence), \
                    mock.patch.object(owner.semver_evidence, "witness_cases",
                        side_effect=lambda current: original_cases(current) if full else {"old_enum": original_cases(current)["old_enum"]}), \
                    mock.patch.object(Path, "write_text", write), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                if not external_spec:
                    _, _, source = owner.sdk_source_observation()
                    if raw == "mismatch":
                        source["working_subject"] = "another-source"
                    spec.write_text(json.dumps({"source": source, "feature": feature,
                        "target": owner.SDK_TARGET, "baseline_rev": "c" * 40}))
                yield {"state": state, "spec": spec, "run": spawn, "capture": capture, "observe": observation}

        @contextlib.contextmanager
        def active_subject(self):
            """The original private Git fixture with the actual2.0 cohort."""
            with tempfile.TemporaryDirectory() as temporary:
                helper = owner.SdkConsumerTests()
                root = helper.repository(Path(temporary))
                for path in (root / "crates").glob("*/Cargo.toml"):
                    path.unlink()
                    path.parent.rmdir()
                for current in (False, True):
                    files = major_source_files(current)
                    for number in range(22 - len(evidence.consumer_package_names())):
                        name = "fixture-sdk-" + str(number).zfill(2)
                        files["crates/" + name + "/Cargo.toml"] = ('[package]\nname=' + json.dumps(name)
                            + '\nversion="1.0.0"\n[package.metadata.awaken]\nproduct="sdk"\napi_snapshot=true\n')
                    # This source's selected SDK normal/build closure contains
                    # no K8s; the existing input owner must establish N/A from
                    # actual committed manifests and lock, not a mocked policy.
                    files["Cargo.lock"] = 'version=4\n' + ''.join(
                        '\n[[package]]\nname=' + json.dumps(name) + '\nversion=' + json.dumps(
                            versions[int(current)]) + '\n' for name, versions in evidence.COHORT.items())
                    for name, value in files.items():
                        destination = root / name
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        destination.write_text(value)
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private " + ("current" if current else "baseline") + " major cohort")
                yield helper, root

        def active_execute(self, helper, root, *, approved=True, failure=None, output_cause="", metadata_cause="", witness_cause=""):
            """Reuse the original outer fixture and the one existing action."""
            observations = []
            def action(command, **arguments):
                spec = Path(command[-1])
                feature = json.loads(spec.read_text())["feature"]
                selected_failure = failure and (feature, "consumers") == (failure["feature"], failure["stage"])
                if selected_failure and failure["raw"] != 0:
                    status = failure["raw"]
                    if failure.get("source"):
                        (root / "tracked.txt").write_text("changed active-major source\n")
                else:
                    with self.major_action(root, 101, compiler=True, full=True, external_spec=spec) as inner:
                        status = owner.run_major_consumer_audit(inner["spec"])
                        observations.append(list(inner["state"]["rows"]))
                    if selected_failure and failure.get("source"):
                        (root / "tracked.txt").write_text("changed active-major source\n")
                    if witness_cause:
                        path = spec.parent / (feature + "-major-consumers/commands.json")
                        receipt = json.loads(path.read_text())
                        if witness_cause == "target": receipt["target"] = "foreign"
                        elif witness_cause == "profile": receipt["feature"] = "all" if feature == "default" else "default"
                        elif witness_cause == "source": receipt["source"]["working_subject"] = "foreign"
                        elif witness_cause == "steps": receipt["steps"] = []
                        path.write_text(json.dumps(receipt))
                return subprocess.CompletedProcess(command, status, "controlled original major action\n", "")

            original_write = Path.write_text
            faulted = []
            def write(path, value, *arguments, **options):
                if failure and failure.get("record") and not faulted:
                    feature, stage = failure["feature"], failure["stage"]
                    label = feature + ("-major-" + stage if stage != "standard" else "")
                    outer = path.parent.name.startswith("sdk-qualification-")
                    if outer and (path.name == label + ".stdout.log" if failure["record"] == "log"
                                  else path.name == "commands.json"):
                        # The journal contains nested witness rows. The real
                        # source of stage identity is the latest outer command,
                        # not the number of nested raw observations.
                        if failure["record"] == "journal":
                            rows = json.loads(value).get("commands", [])
                            if not rows:
                                return original_write(path, value, *arguments, **options)
                            command = rows[-1]["command"]
                            actual = "consumers" if "--major-consumer-audit" in command else "audit" if "--release-type" in command else "standard"
                            actual_feature = (json.loads(Path(command[-1]).read_text())["feature"] if actual == "consumers"
                                              else "all" if "--all-features" in command else "default")
                            if (actual_feature, actual) != (feature, stage):
                                return original_write(path, value, *arguments, **options)
                        faulted.append(path)
                        raise OSError("exact active-major evidence refusal")
                return original_write(path, value, *arguments, **options)
            known, _ = SdkSemverEvidenceTests().fixture()
            with mock.patch.object(evidence, "APPROVED_FINDINGS", known if approved else None), \
                    mock.patch.object(Path, "write_text", write):
                status, calls = helper.execute(root, qualification="semver", active_major=True,
                    major_action_run=action, major_failure=failure, output_cause=output_cause, metadata_cause=metadata_cause)
            path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"), None)
            return status, calls, json.loads(path.read_text()) if path else None, observations

        @staticmethod
        def release_calls(calls):
            return [command for command in calls if "--major-consumer-audit" in command
                    or command[:3] == ["cargo", "semver-checks", "check-release"]]

        def test_existing_test_action_keeps_source_refusal_and_original_archive_failure(self):
            # D7 the existing action uses that same observation/merged adapter.
            # Clean+matching source+archive1/23/75/101/signal => original status
            # and one real argv receipt. D8 dirty/mismatched source =>1 before
            # any subprocess or work directory. No compiler-success or major
            # qualification is fabricated by these controlled early-failure rows.
            for raw in (1, 23, 75, 101, -15, None, "mismatch"):
                with self.subTest(raw=raw), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    with self.major_action(root, raw) as action:
                        spec = action["spec"]
                        run, capture, observe = action["run"], action["capture"], action["observe"]
                        result = owner.run_major_consumer_audit(spec)
                    self.assertEqual(result, (143 if raw == -15 else raw) if isinstance(raw, int) else 1)
                    if isinstance(raw, int):
                        self.assertEqual(run.call_count, 1)
                        self.assertEqual(capture.call_count, 1)
                        self.assertIs(capture.call_args.kwargs["merged"], True)
                        self.assertEqual(run.call_args.args[0][:3], ["git", "archive", "--format=tar"])
                        self.assertEqual(run.call_args.args[0][-1], "c" * 40)
                        journal = json.loads((spec.parent / "default-major-consumers/commands.json").read_text())
                        self.assertEqual(journal["steps"][0]["exit_code"], 143 if raw == -15 else raw)
                    else:
                        run.assert_not_called()
                        capture.assert_not_called()
                        self.assertFalse((spec.parent / "default-major-consumers").exists())
                    self.assertGreater(observe.call_count, 1)

        def test_step_retention_fault_keeps_the_original_status_and_stops_the_action(self):
            # J1-J3 cause/effect table: child0/75/ordinary101/signal143 crossed
            # with post-source/log/journal failure => raw nonzero wins (0->1),
            # one archive command, no extraction/metadata/compiler successor.
            # Journal IO belongs inside the original record boundary: a write
            # fault after75 must not escape to a second writer and become1.
            # A missing log must also not update a different/absent step row.
            # J8 log IO failure+available journal -> retain this last raw row
            # once; missing output still refuses the action and any successor.
            for raw in (0, 75, 101, -15):
                for cause in ("source", "log", "journal"):
                    with self.subTest(raw=raw, cause=cause), tempfile.TemporaryDirectory() as temporary:
                        with self.major_action(Path(temporary), raw, cause=cause) as action:
                            status = owner.run_major_consumer_audit(action["spec"])
                        self.assertEqual(status, 143 if raw == -15 else raw or 1)
                        self.assertEqual(action["run"].call_count, 1)
                        row, = action["state"]["rows"]
                        self.assertEqual(row["exit_code"], 143 if raw == -15 else raw)
                        self.assertIn("subject_error" if cause == "source" else "record_error", row)
                        self.assertEqual(action["state"]["journal_writes"], 1)
                        if cause == "log":
                            retained = json.loads((action["spec"].parent /
                                "default-major-consumers/commands.json").read_text())
                            self.assertEqual(retained["steps"][-1]["command"], row["command"])
                            self.assertEqual(retained["steps"][-1]["exit_code"], row["exit_code"])

        def test_expected_compiler_refusal_requires_successful_source_and_evidence_retention(self):
            # J4 exact compiler101+code/span/source+retained raw channels and
            # journal => asserted negative data; the action alone returns0.
            # J5-J7 the same diagnostic + source/log/journal uncertainty =>101,
            # even if compiler_result already set accepted=True. No generic
            # assessor may absorb101. Log failure still records the same raw
            # step once in the available journal; incomplete channels refuse.
            # This minimal private two-step compiler fixture tests retention
            # only, not real SDK/baseline qualification.
            for cause in ("", "source", "log", "journal"):
                with self.subTest(cause=cause), tempfile.TemporaryDirectory() as temporary:
                    with self.major_action(Path(temporary), 101, cause=cause, compiler=True) as action:
                        status = owner.run_major_consumer_audit(action["spec"])
                    self.assertEqual(status, 101 if cause else 0)
                    row = action["state"]["rows"][-1]
                    self.assertEqual(row["exit_code"], 101)
                    if cause:
                        self.assertIn("subject_error" if cause == "source" else "record_error", row)
                        self.assertEqual(action["run"].call_args.args[0][1], "check")
                        self.assertEqual(action["state"]["journal_writes"], 1)
                        if cause == "log":
                            retained = json.loads((action["spec"].parent /
                                "default-major-consumers/commands.json").read_text())
                            self.assertEqual(retained["steps"][-1]["command"], row["command"])
                            self.assertEqual(retained["steps"][-1]["exit_code"], 101)
                    else:
                        path = action["spec"].parent / "default-major-consumers/commands.json"
                        receipt = json.loads(path.read_text())
                        self.assertEqual(receipt["exit_code"], 0)
                        self.assertEqual(receipt["steps"][-1]["exit_code"], 101)
                        self.assertEqual(len(receipt["cases"]), 2)
                        self.assertNotIn("record_error", row)
                        self.assertEqual(action["state"]["journal_writes"], 2)

        def test_complete_major_witness_profiles_bind_nine_cases_fourteen_steps_and_retained_bytes(self):
            # Active-major S2-S5 + Runtime R1-R5: both profiles retain original9
            # cases/14 core steps +5 Runtime controls retain14/19, then Contract
            # adds baseline-old/current-negative/migration-positive ->17/22.
            # Designed raw101 counts remain3 core,5 old Runtime subject,6 full.
            # W38 appends baseline inventory, five independently failing old route
            # shapes on both sides, and canonical Agent/Tool positive controls.
            # Original subsets below still assert9/14,14/19,17/22 and raw101 laws.
            # Retained W38 subject remains30 cases/35 steps,11 raw negatives.
            # I1-I8 append four baseline positives, four current reason-specific
            # negatives and four canonical positives. Counts derive from the
            # same tables; mock statuses are never real compiler observations.
            # These are controlled design observations, not real compiler results.
            # Metadata carries each side's four major +two ordinary owners; these
            # controlled observations never fill production approval or receipts.
            # Each side supplies its own declared features;
            # all enables those dependency keys, while default keeps defaults.
            # S4 every single receipt/source/output mutation below refuses;
            # a case claim alone is never a compiler observation.
            declared = {side: {name: {} for name in evidence.COHORT} for side in ("baseline", "current")}
            declared["baseline"]["awaken-provider-genai"] = {"old_feature": []}
            declared["current"]["awaken-provider-genai"] = {"new_feature": []}
            for feature in ("default", "all"):
                with self.subTest(feature=feature), tempfile.TemporaryDirectory() as temporary:
                    with self.major_action(Path(temporary), 101, compiler=True, full=True,
                                           feature=feature, declared=declared) as action:
                        self.assertEqual(owner.run_major_consumer_audit(action["spec"]), 0)
                        work = action["spec"].parent / (feature + "-major-consumers")
                        receipt = json.loads((work / "commands.json").read_text())
                        runtime_names = {"old_runtime_type", "old_runtime_method", "migrated_runtime"}
                        contract_names = {"old_retry_policy_decide", "migrated_retry_policy_decide"}
                        inference_cases = [row for row in receipt["cases"] if row["case"].startswith("inference_")]
                        retained_cases = [row for row in receipt["cases"] if not row["case"].startswith("inference_")]
                        retained_steps = [row for row in receipt["steps"] if "--bin" not in row["command"]
                            or not row["command"][row["command"].index("--bin") + 1].startswith("inference_")]
                        original_cases = [row for row in retained_cases if not row["case"].startswith("owner_")]
                        original_steps = [row for row in retained_steps if "--bin" not in row["command"]
                            or not row["command"][row["command"].index("--bin") + 1].startswith("owner_")]
                        runtime_cases = [row for row in original_cases if row["case"] not in contract_names]
                        runtime_steps = [row for row in original_steps if "--bin" not in row["command"]
                            or row["command"][row["command"].index("--bin") + 1] not in contract_names]
                        core_cases = [row for row in runtime_cases if row["case"] not in runtime_names]
                        core_steps = [row for row in runtime_steps if "--bin" not in row["command"]
                            or row["command"][row["command"].index("--bin") + 1] not in runtime_names]
                        self.assertEqual((len(core_cases), len(core_steps)), (9, 14))
                        self.assertEqual(sum(row["exit_code"] == 101 for row in core_steps), 3)
                        self.assertEqual((len(runtime_cases), len(runtime_steps)), (14, 19))
                        self.assertEqual(sum(row["exit_code"] == 101 for row in runtime_steps), 5)
                        self.assertEqual((len(original_cases), len(original_steps)), (17, 22))
                        self.assertEqual((len(retained_cases), len(retained_steps)), (30, 35))
                        self.assertEqual(sum(row["exit_code"] == 101 for row in original_steps), 6)
                        self.assertEqual(sum(row["exit_code"] == 101 for row in retained_steps), 11)
                        pairs = (
                            ("inference_old_disposition", "inference_migrated_disposition"),
                            ("inference_old_error", "inference_migrated_error"),
                            ("inference_old_gate", "inference_migrated_gate"),
                            ("inference_old_runtime_ceiling", "inference_migrated_runtime_policy"),
                        )
                        expected_inference = {
                            **{("baseline", old): 0 for old, _ in pairs},
                            **{("current", old): 101 for old, _ in pairs},
                            **{("current", migrated): 0 for _, migrated in pairs},
                        }
                        self.assertEqual({(row["side"], row["case"]): row["original_exit_code"]
                                          for row in inference_cases}, expected_inference)
                        self.assertEqual((len(receipt["cases"]), len(receipt["steps"])),
                                         (len(retained_cases) + len(expected_inference),
                                          len(retained_steps) + len(expected_inference)))
                        self.assertEqual(sum(row["exit_code"] == 101 for row in receipt["steps"]), 11 + len(pairs))
                        contract_results = {(row["side"], row["case"]): row["original_exit_code"]
                                            for row in receipt["cases"] if row["case"] in contract_names}
                        self.assertEqual(contract_results, {
                            ("baseline", "old_retry_policy_decide"): 0,
                            ("current", "old_retry_policy_decide"): 101,
                            ("current", "migrated_retry_policy_decide"): 0,
                        })
                        runtime_results = {(row["side"], row["case"]): row["original_exit_code"]
                                           for row in receipt["cases"] if row["case"] in runtime_names}
                        self.assertEqual(runtime_results, {
                            ("baseline", "old_runtime_type"): 0, ("baseline", "old_runtime_method"): 0,
                            ("current", "old_runtime_type"): 101, ("current", "old_runtime_method"): 101,
                            ("current", "migrated_runtime"): 0,
                        })
                        self.assertEqual(receipt["steps"][0]["command"], ["git", "archive", "--format=tar", "--output", str(work / "baseline.tar"), receipt["baseline_rev"]])
                        for row in receipt["steps"][1:]:
                            command = row["command"]
                            self.assertEqual("--all-features" in command, feature == "all")
                            if command[1] == "metadata":
                                producer = "-consumer" not in Path(command[command.index("--manifest-path") + 1]).parent.name
                                self.assertEqual("--locked" in command, producer)
                                self.assertIn("--format-version", command)
                                self.assertNotIn("--no-deps", command)
                            else:
                                self.assertEqual(command[:4], ["cargo", "check", "--locked", "--manifest-path"])
                                self.assertEqual(command[6:8], [owner.SDK_TARGET, "--bin"])
                                self.assertIn("--message-format=json", command)
                        for side in ("baseline", "current"):
                            expected = sorted(declared[side]["awaken-provider-genai"]) if feature == "all" else []
                            self.assertEqual(receipt["sides"][side]["requested_sdk_features"]["awaken-provider-genai"], expected)
                            self.assertNotIn("bedrock-sigv4", receipt["sides"][side]["genai_features"])
                        arguments = (work, receipt["source"], receipt["baseline_rev"], feature, owner.SDK_TARGET)
                        owner.validate_retained_major_receipt(receipt, *arguments)
                        for cause in ("target", "side", "manifest-hash", "lock-hash", "step-missing", "step-argv", "step-status", "step-log", "case-extra", "case-status", "case-diagnostic"):
                            bad = copy.deepcopy(receipt)
                            if cause == "target": bad["target"] = "foreign"
                            elif cause == "side": bad["sides"]["current"]["genai"]["source"] = "foreign"
                            elif cause in ("manifest-hash", "lock-hash"): bad["sides"]["current"][cause.replace("-hash", "_sha256")] = "0" * 64
                            elif cause == "step-missing": bad["steps"].pop()
                            elif cause == "step-argv": bad["steps"][-1]["command"].append("--foreign")
                            elif cause == "step-status": bad["steps"][-1]["exit_code"] = 75
                            elif cause == "step-log": bad["steps"][-1]["log_sha256"] = "0" * 64
                            elif cause == "case-extra": bad["cases"].append(copy.deepcopy(bad["cases"][0]))
                            elif cause == "case-status": bad["cases"][-1]["original_exit_code"] = 101
                            else:
                                original_negative = next(row for row in bad["cases"]
                                    if (row["side"], row["case"]) == ("current", "old_enum"))
                                original_negative["compiler_errors"][0]["code"]["code"] = "E9999"
                            with self.subTest(cause=cause), self.assertRaises((ValueError, OSError)):
                                owner.validate_retained_major_receipt(bad, *arguments)
                        for relative in ("current-consumer/Cargo.toml", "current-consumer/Cargo.lock",
                                         "current-consumer/cases/migrated_provider.rs", "current-old_enum.stdout.log",
                                         "current-consumer/cases/old_retry_policy_decide.rs",
                                         "current-consumer/cases/migrated_retry_policy_decide.rs",
                                         "current-consumer/cases/inference_old_gate.rs",
                                         "current-consumer/cases/inference_migrated_disposition.rs"):
                            path = work / relative
                            original = path.read_bytes()
                            path.write_bytes(original + b"\nchanged retained data\n")
                            with self.subTest(relative=relative), self.assertRaises((ValueError, OSError)):
                                owner.validate_retained_major_receipt(receipt, *arguments)
                            path.write_bytes(original)

        def test_major_metadata_alias_graph_and_declared_features_are_not_interchangeable(self):
            # Active-major S3: SDK roots/version/unique reachable identities,
            # actual requested dependency features and both deliberate genai
            # Client aliases are conjunctive. One unknown/foreign/missing edge
            # or enabled Bedrock variant invalidates the fixed input; it must
            # not be repaired by wildcarding the enum or unifying Client types.
            # Runtime/Contract retain their original source/version/path/feature
            # axes; W38 keeps the same axes for Agent/Tool defining dependencies.
            # Their separately declared versions cannot erase this owner role.
            # Missing/foreign/unreached/feature/version failures remain refusal.
            # Both owners remain transitively reached through Contract when the
            # consumer direct edge is removed or changed to dev-only. Those rows
            # must still refuse: transitive reachability cannot compile an import.
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                consumer = root / "consumer"
                declared = {name: {"chosen": []} for name in evidence.consumer_package_names()}
                producer = major_metadata_fixture(root, current=True, feature="all", declared=declared)
                members = {p["name"]: p for p in producer["packages"] if p["name"] in evidence.consumer_package_names()}
                good = major_metadata_fixture(root, current=True, feature="all", consumer=consumer, declared=declared)
                evidence.validate_consumer_metadata(good, consumer, members, current=True, feature="all")
                for cause in ("duplicate-package", "duplicate-node", "unresolved", "unreached", "feature", "malformed-feature", "sdk-source", "provider-source", "old-client", "registry-client", "bedrock", "foreign-root", "runtime-missing", "runtime-feature", "runtime-version", "runtime-path", "contract-missing", "contract-feature", "contract-version", "contract-path",
                              "agent-missing", "agent-dev", "agent-feature", "agent-version", "agent-path",
                              "tool-missing", "tool-dev", "tool-feature", "tool-version", "tool-path"):
                    bad = copy.deepcopy(good)
                    nodes = {node["id"]: node for node in bad["resolve"]["nodes"]}
                    if cause == "duplicate-package": bad["packages"].append(copy.deepcopy(bad["packages"][0]))
                    elif cause == "duplicate-node": bad["resolve"]["nodes"].append(copy.deepcopy(bad["resolve"]["nodes"][0]))
                    elif cause == "unresolved": nodes["consumer"]["deps"].append({"name": "unknown", "pkg": "missing"})
                    elif cause == "unreached": nodes["consumer"]["deps"] = [dep for dep in nodes["consumer"]["deps"] if dep["pkg"] != "current:awaken-ext-builtin-tools"]
                    elif cause == "feature": nodes["current:awaken-provider-genai"]["features"] = []
                    elif cause == "malformed-feature": nodes["genai-current"]["features"] = "default"
                    elif cause == "sdk-source": bad["packages"][0]["source"] = "git+foreign"
                    elif cause == "provider-source": nodes["current:awaken-provider-genai"]["deps"][0]["pkg"] = "genai-old"
                    elif cause in ("old-client", "registry-client"):
                        alias = "genai_old" if cause == "old-client" else "genai"
                        next(dep for dep in nodes["consumer"]["deps"] if dep["name"] == alias)["pkg"] = "genai-current" if cause == "old-client" else "genai-old"
                    elif cause == "bedrock": nodes["genai-current"]["features"].append("bedrock-sigv4")
                    elif cause.startswith(("runtime-", "contract-", "agent-", "tool-")):
                        prefix, fault = cause.split("-", 1)
                        name = {"runtime": "awaken-runtime", "contract": "awaken-runtime-contract",
                                "agent": "awaken-agent-contract", "tool": "awaken-tool-contract"}[prefix]
                        if fault == "missing":
                            nodes["consumer"]["deps"] = [dep for dep in nodes["consumer"]["deps"]
                                                          if dep["pkg"] != "current:" + name]
                        elif fault == "dev":
                            next(dep for dep in nodes["consumer"]["deps"]
                                 if dep["pkg"] == "current:" + name)["dep_kinds"] = [{"kind": "dev", "target": None}]
                        elif fault == "feature": nodes["current:" + name]["features"] = []
                        else:
                            package = next(package for package in bad["packages"] if package["name"] == name)
                            package["version" if fault == "version" else "manifest_path"] = (
                                ("9.0.0" if prefix in ("agent", "tool") else "1.0.0")
                                if fault == "version" else str(root / "unrelated/Cargo.toml"))
                    else: bad["workspace_root"] = str(root / "foreign")
                    with self.subTest(cause=cause), self.assertRaises(ValueError):
                        evidence.validate_consumer_metadata(bad, consumer, members, current=True, feature="all")

            # U1/U2 cause-effect table: disjoint roles, or an independently
            # declared release that also defines a witness type, produce one
            # authored dependency/package/node with the side's exact version.
            # U3 overlap never removes the defining-owner direct-normal rule:
            # a dev-only consumer edge refuses even while Contract reaches the
            # same package transitively. Both sides/profiles are controlled
            # protocol inputs here, not Cargo or release qualification evidence.
            for overlap in (False, True):
                versions = {name: pair for name, pair in evidence.COHORT.items() if name not in evidence.WITNESS_OWNERS}
                if overlap:
                    versions.update({name: ("1.0.0", "2.0.0") for name in evidence.WITNESS_OWNERS})
                with mock.patch.dict(evidence.COHORT, versions, clear=True), tempfile.TemporaryDirectory() as temporary:
                    names = evidence.consumer_package_names()
                    self.assertEqual(len(names), len(set(names)))
                    self.assertEqual(set(names), set(evidence.COHORT) | set(evidence.WITNESS_OWNERS))
                    for current in (False, True):
                        for feature in ("default", "all"):
                            root = Path(temporary) / ("current" if current else "baseline") / feature
                            for relative, value in major_source_files(current).items():
                                path = root / relative
                                path.parent.mkdir(parents=True, exist_ok=True)
                                path.write_text(value)
                            consumer = root.parent / (feature + "-consumer")
                            producer = major_metadata_fixture(root, current=current, feature=feature)
                            members = owner.major_producer_packages(producer, root, current=current)
                            manifest = owner.tomllib.loads(evidence.consumer_manifest(members, current=current, feature=feature))
                            self.assertTrue(set(names).issubset(manifest["dependencies"]))
                            self.assertEqual(len([p for p in producer["packages"] if p["name"] in names]), len(names))
                            good = major_metadata_fixture(root, current=current, feature=feature, consumer=consumer)
                            observed = evidence.validate_consumer_metadata(good, consumer, members, current=current, feature=feature)
                            self.assertEqual(set(observed["sdk_packages"]), set(names))
                            for name in evidence.WITNESS_OWNERS:
                                package = members[name]
                                self.assertEqual(manifest["dependencies"][name]["version"], "=" + package["version"])
                                bad = copy.deepcopy(good)
                                node = next(n for n in bad["resolve"]["nodes"] if n["id"] == "consumer")
                                next(edge for edge in node["deps"] if edge["name"] == name.replace("-", "_"))["dep_kinds"] = [{"kind": "dev", "target": None}]
                                with self.subTest(overlap=overlap, current=current, feature=feature, owner=name), self.assertRaisesRegex(ValueError, "direct normal dependency"):
                                    evidence.validate_consumer_metadata(bad, consumer, members, current=current, feature=feature)

        def test_producer_member_identity_must_be_authored_inside_the_observed_side(self):
            # S2: valid metadata is not authority to rename another manifest.
            # Same-root unrelated/wrong-name manifest, outside-root source,
            # wrong version or foreign workspace => refuse before rendering a
            # consumer. This reuses the producer predicate on both profiles;
            # no fallback member path or second manifest selection is allowed.
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "source"
                for relative, value in major_source_files(True).items():
                    path = root / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(value)
                good = major_metadata_fixture(root, current=True, feature="default")
                self.assertEqual(set(owner.major_producer_packages(good, root, current=True)), set(evidence.consumer_package_names()))
                for cause in ("foreign-root", "outside-path", "unrelated-path", "wrong-version", "authored-name", "authored-version"):
                    bad = copy.deepcopy(good)
                    package = bad["packages"][0]
                    path = Path(package["manifest_path"])
                    original = path.read_text()
                    if cause == "foreign-root": bad["workspace_root"] = str(root.parent)
                    elif cause == "outside-path":
                        outside = root.parent / "Cargo.toml"
                        outside.write_text(original)
                        package["manifest_path"] = str(outside)
                    elif cause == "unrelated-path": package["manifest_path"] = str(root / "Cargo.toml")
                    elif cause == "wrong-version": package["version"] = "3.0.0"
                    elif cause == "authored-name": path.write_text(original.replace(package["name"], "unrelated-sdk"))
                    else: path.write_text(original.replace('"2.0.0"', '"3.0.0"'))
                    try:
                        with self.subTest(cause=cause), self.assertRaises((ValueError, KeyError)):
                            owner.major_producer_packages(bad, root, current=True)
                    finally:
                        path.write_text(original)
                # O1-O4: a defining owner's authored/metadata identity must stay
                # exact even when it also belongs to the release cohort. Change
                # only its identity/version; every other member remains exact.
                for name in evidence.WITNESS_OWNERS:
                    for cause in ("authored-name", "authored-version", "metadata-version", "outside-path"):
                        bad = copy.deepcopy(good)
                        package = next(package for package in bad["packages"] if package["name"] == name)
                        path = Path(package["manifest_path"])
                        original = path.read_text()
                        if cause == "authored-name": path.write_text(original.replace(name, "unrelated-owner", 1))
                        elif cause == "authored-version":
                            authored_version = next(line for line in original.splitlines() if line.startswith("version"))
                            path.write_text(original.replace(authored_version, 'version="9.0.0"', 1))
                        elif cause == "metadata-version": package["version"] = "9.0.0"
                        else:
                            outside = root.parent / (name + ".toml")
                            outside.write_text(original)
                            package["manifest_path"] = str(outside)
                        try:
                            with self.subTest(owner=name, cause=cause), self.assertRaises((ValueError, KeyError)):
                                owner.major_producer_packages(bad, root, current=True)
                        finally:
                            path.write_text(original)
                # Exact50b3 stable members inherit workspace1.0; the three
                # 0.1 members author their literal versions. Metadata cannot
                # substitute for missing/wrong authored version or inheritance.
                for relative, value in major_source_files(False).items():
                    (root / relative).write_text(value)
                baseline = major_metadata_fixture(root, current=False, feature="all")
                self.assertEqual(set(owner.major_producer_packages(baseline, root, current=False)), set(evidence.consumer_package_names()))
                workspace = root / "Cargo.toml"
                original_root = workspace.read_text()
                path = Path(baseline["packages"][0]["manifest_path"])
                original = path.read_text()
                for cause in ("missing-root", "missing-version", "wrong-version", "false-inheritance", "extra-inheritance", "wrong-name"):
                    if cause == "missing-root": workspace.unlink()
                    elif cause == "missing-version": workspace.write_text('[workspace]\nmembers=["crates/*"]\n')
                    elif cause == "wrong-version": workspace.write_text(original_root.replace('"1.0.0"', '"3.0.0"'))
                    elif cause == "false-inheritance": path.write_text(original.replace("workspace=true", "workspace=false"))
                    elif cause == "extra-inheritance": path.write_text(original.replace("workspace=true", "workspace=true\nversion.extra=true"))
                    else: path.write_text(original.replace(baseline["packages"][0]["name"], "unrelated-sdk"))
                    try:
                        with self.subTest(baseline=cause), self.assertRaises((ValueError, OSError, KeyError)):
                            owner.major_producer_packages(baseline, root, current=False)
                    finally:
                        workspace.write_text(original_root)
                        path.write_text(original)

                # V1/V2: baseline 0.1 -> current 0.2 is independently authored;
                # changing only the baseline declaration to workspace1 refuses
                # despite unchanged claimed0.1 metadata. A graph/version label
                # cannot stand in for the package's real source declaration.
                for name, (old_version, _) in evidence.COHORT.items():
                    if old_version == "1.0.0":
                        continue
                    package = next(p for p in baseline["packages"] if p["name"] == name)
                    path = Path(package["manifest_path"])
                    original = path.read_text()
                    path.write_text(original.replace('version=' + json.dumps(old_version), "version.workspace=true", 1))
                    try:
                        with self.subTest(package=name, old_version=old_version), self.assertRaises(ValueError):
                            owner.major_producer_packages(baseline, root, current=False)
                    finally:
                        path.write_text(original)

        def test_complete_witness_stops_on_owned_drift_primary_failure_or_unsafe_archive(self):
            # Active-major S2/S5/S8: the full path, not only its reduced record
            # fixture, retains ordinary75/101/signal143 and refuses successful
            # children followed by owned baseline/source/lock drift. Expected101
            # is accepted only with the corresponding source/record intact.
            for feature in ("default", "all"):
                for step in ("baseline-sdk-metadata", "baseline-old_client", "current-migrated_provider"):
                    for raw in (75, 101, -15):
                        with self.subTest(feature=feature, step=step, raw=raw), tempfile.TemporaryDirectory() as temporary:
                            with self.major_action(Path(temporary), raw, compiler=True, full=True, feature=feature, failure_step=step) as action:
                                status = owner.run_major_consumer_audit(action["spec"])
                            self.assertEqual(status, 143 if raw == -15 else raw)
                            self.assertEqual(action["state"]["rows"][-1]["exit_code"], 143 if raw == -15 else raw)
                for drift in ("consumer-source", "consumer-lock", "baseline-source"):
                    with self.subTest(feature=feature, drift=drift), tempfile.TemporaryDirectory() as temporary:
                        with self.major_action(Path(temporary), 0, compiler=True, full=True, feature=feature,
                            failure_step="current-migrated_provider", drift=drift) as action:
                            status = owner.run_major_consumer_audit(action["spec"])
                        self.assertEqual(status, 1)
                        self.assertIn("subject_error", action["state"]["rows"][-1])
                for cause in ("source", "log", "journal"):
                    with self.subTest(feature=feature, cause=cause), tempfile.TemporaryDirectory() as temporary:
                        with self.major_action(Path(temporary), 101, compiler=True, full=True, feature=feature,
                            failure_step="current-old_client", cause=cause) as action:
                            status = owner.run_major_consumer_audit(action["spec"])
                        self.assertEqual(status, 101)
                        self.assertIn("subject_error" if cause == "source" else "record_error", action["state"]["rows"][-1])
            for cause in ("escape", "link"):
                with self.subTest(archive=cause), tempfile.TemporaryDirectory() as temporary:
                    with self.major_action(Path(temporary), 101, compiler=True, full=True, archive_cause=cause) as action:
                        self.assertEqual(owner.run_major_consumer_audit(action["spec"]), 1)
                        self.assertEqual(action["run"].call_count, 1)
                        self.assertFalse((action["spec"].parent / "default-major-consumers/outside").exists())

        def test_runtime_controls_preserve_original_status_across_source_and_record_faults(self):
            # R8 cause/effect table: each deliberate Runtime101 + intact source
            # and retained output/journal -> action0; any source/log/journal
            # fault -> original101, never an accepted unretained negative.
            # Canonical positive0 + fault ->1; ordinary75/101/signal143 keeps
            # its original status above every secondary fault. The existing
            # major_action and execute/record owners supply every observation.
            # I1-I8 use the same fault table: new exact-reason101 requires all
            # retained source/log/journal facts; canonical positives cannot
            # absorb ordinary failures. These remain mocked design controls.
            # R9: wrong diagnostic+available log/journal -> retain the final raw
            # step and101, with no accepted case; one existing writer owns it.
            for feature in ("default", "all"):
                for case in ("old_runtime_type", "old_runtime_method", "old_retry_policy_decide",
                             "inference_old_disposition", "inference_old_error", "inference_old_gate",
                             "inference_old_runtime_ceiling"):
                    for cause in ("", "source", "log", "journal", "code"):
                        with self.subTest(feature=feature, case=case, cause=cause), tempfile.TemporaryDirectory() as temporary:
                            with self.major_action(Path(temporary), 101, compiler=True, full=True,
                                feature=feature, failure_step="current-" + case, cause=cause) as action:
                                status = owner.run_major_consumer_audit(action["spec"])
                            self.assertEqual(status, 101 if cause else 0)
                            if cause:
                                self.assertEqual(action["state"]["rows"][-1]["exit_code"], 101)
                                self.assertIn("subject_error" if cause == "source" else "record_error",
                                              action["state"]["rows"][-1])
                            if cause == "code":
                                retained = json.loads((action["spec"].parent /
                                    (feature + "-major-consumers") / "commands.json").read_text())
                                self.assertEqual(retained["steps"][-1]["command"],
                                                 action["state"]["rows"][-1]["command"])
                                self.assertEqual(retained["steps"][-1]["exit_code"], 101)
                                self.assertFalse(any(row["side"] == "current" and row["case"] == case
                                                     for row in retained["cases"]))
                for migration in ("migrated_runtime", "migrated_retry_policy_decide",
                                  "inference_migrated_disposition", "inference_migrated_error",
                                  "inference_migrated_gate", "inference_migrated_runtime_policy"):
                    for raw in (0, 75, 101, -15):
                        for cause in ("", "source", "log", "journal"):
                            with self.subTest(feature=feature, migration=migration, raw=raw, cause=cause), tempfile.TemporaryDirectory() as temporary:
                                with self.major_action(Path(temporary), raw, compiler=True, full=True,
                                    feature=feature, failure_step="current-" + migration, cause=cause) as action:
                                    status = owner.run_major_consumer_audit(action["spec"])
                                original = 143 if raw == -15 else raw
                                self.assertEqual(status, original or (1 if cause else 0))
                                self.assertEqual(action["state"]["rows"][-1]["exit_code"], original)

        def test_active_major_cohort_runs_original_six_items_and_twenty_two_selectors(self):
            # Active-major S1/S5-S7 + R1-R7/U4: every declared release package
            # plus its unique remaining peers -> the original compiler controls,
            # exact forced reports and all22 standard selectors. The private
            # Git fixture and metadata use the same role union: overlap cannot
            # omit a baseline package or duplicate its manifest identity for
            # default then all. The approved set below is controlled fixture
            # data only; production findings come from separately retained and
            # reviewed real reports. Raw audit1 stays1; only its explicit
            # semantic result is0. Every original SDK22 argv remains intact.
            with self.active_subject() as (helper, root):
                status, calls, receipt, observations = self.active_execute(helper, root)
                self.assertEqual(status, 0)
                releases = self.release_calls(calls)
                self.assertEqual(len(releases), 6)
                runtime_names = {"old_runtime_type", "old_runtime_method", "migrated_runtime"}
                contract_names = {"old_retry_policy_decide", "migrated_retry_policy_decide"}
                retained_observations = [[row for row in rows if "--bin" not in row["command"]
                    or not row["command"][row["command"].index("--bin") + 1].startswith("inference_")]
                    for rows in observations]
                original_observations = [[row for row in rows if "--bin" not in row["command"]
                    or not row["command"][row["command"].index("--bin") + 1].startswith("owner_")]
                    for rows in retained_observations]
                runtime_observations = [[row for row in rows if "--bin" not in row["command"]
                    or row["command"][row["command"].index("--bin") + 1] not in contract_names]
                    for rows in original_observations]
                core_observations = [[row for row in rows if "--bin" not in row["command"]
                    or row["command"][row["command"].index("--bin") + 1] not in runtime_names]
                    for rows in runtime_observations]
                self.assertEqual([len(rows) for rows in core_observations], [14, 14])
                self.assertEqual([len(rows) for rows in runtime_observations], [19, 19])
                self.assertEqual([len(rows) for rows in original_observations], [22, 22])
                self.assertEqual([len(rows) for rows in retained_observations], [35, 35])
                expected_steps = evidence.witness_steps(root, root / "planned", "c" * 40, "default", owner.SDK_TARGET)
                self.assertEqual([len(rows) for rows in observations], [len(expected_steps)] * 2)
                self.assertEqual(len(receipt["sdk_crates"]), 22)
                self.assertEqual(set(receipt["major_audits"]), {"default", "all"})
                for index, feature in ((0, "default"), (3, "all")):
                    witness, audit, standard = releases[index:index + 3]
                    self.assertEqual(witness[2], "--major-consumer-audit")
                    spec = json.loads(Path(witness[-1]).read_text())
                    self.assertEqual(spec["feature"], feature)
                    self.assertEqual(spec["source"], receipt["source"])
                    command = ["cargo", "semver-checks", "check-release", "--baseline-rev", receipt["baseline_rev"], "--target", owner.SDK_TARGET,
                               "--all-features" if feature == "all" else "--default-features"]
                    self.assertEqual(audit, command + ["--release-type", "patch"] + [word for name in sorted(evidence.COHORT) for word in ("-p", name)])
                    self.assertEqual(standard, command + [word for name in receipt["sdk_crates"] for word in ("-p", name)])
                    row = receipt["commands"][index + 1]
                    self.assertEqual(row["exit_code"], 1)
                    self.assertEqual(row["result_exit_code"], 0)
                    self.assertEqual(row["semantic_result"], "accepted_major")
                self.assertEqual(receipt["compiler_inputs"]["baseline"]["applicability"], "not_applicable")
                self.assertIsNone(receipt["compiler_inputs"]["baseline"]["model"])

        def test_active_major_refuses_unreviewed_reports_or_incomplete_physical_witness(self):
            # S4/S6/S7 conjunctive refusal table: None/unknown/fatal findings,
            # foreign source/profile/target/empty steps, or missing/duplicate/
            # zero nonmajor standard observations =>1 before the dependent
            # item. No successful outer child alone establishes a witness.
            for cause in ("unreviewed", "audit-unknown", "audit-fatal", "target", "profile", "source", "steps", "semver-missing", "semver-duplicate", "semver-zero-checks"):
                with self.subTest(cause=cause), self.active_subject() as (helper, root):
                    status, calls, receipt, _ = self.active_execute(helper, root,
                        approved=cause != "unreviewed",
                        output_cause=cause if cause.startswith(("audit-", "semver-")) else "",
                        witness_cause=cause if cause in ("target", "profile", "source", "steps") else "")
                    self.assertEqual(status, 1)
                    expected = 1 if cause in ("target", "profile", "source", "steps") else 3 if cause.startswith("semver-") else 2
                    self.assertEqual(len(self.release_calls(calls)), expected)
                    self.assertNotIn("all", receipt.get("major_audits", {}))
            for cause in ("major-partial", "major-unknown"):
                with self.subTest(cause=cause), self.active_subject() as (helper, root):
                    status, calls, _, _ = self.active_execute(helper, root, metadata_cause=cause)
                    self.assertEqual(status, 1)
                    self.assertEqual(self.release_calls(calls), [])

        def test_active_major_preserves_stage_status_above_secondary_evidence_failure(self):
            # Active-major S8 cause/effect table: stage=consumer/audit/standard
            # x profile=default/all x raw=0/75/ordinary101/signal143, crossed
            # with source/log/journal fault => original nonzero wins, 0->1,
            # no dependent command. Audit raw1 is additionally tested after
            # semantic acceptance: later evidence uncertainty must revoke0.
            # Expected inner101 remains data only through the prior9/14 proof.
            for feature in ("default", "all"):
                for stage in ("consumers", "audit", "standard"):
                    for raw in (0, 75, 101, -15, *([1] if stage == "audit" else [])):
                        for cause in ("source", "log", "journal"):
                            # A raw0 consumer needs a complete retained witness
                            # before an output fault is tested. Source+raw0 is
                            # still tested at the outer boundary after that IO.
                            with self.subTest(feature=feature, stage=stage, raw=raw, cause=cause), self.active_subject() as (helper, root):
                                failure = {"feature": feature, "stage": stage, "raw": raw}
                                failure["source" if cause == "source" else "record"] = True if cause == "source" else cause
                                status, calls, receipt, _ = self.active_execute(helper, root, failure=failure)
                                self.assertEqual(status, 143 if raw == -15 else raw or 1)
                                expected = (3 if feature == "all" else 0) + {"consumers": 1, "audit": 2, "standard": 3}[stage]
                                self.assertEqual(len(self.release_calls(calls)), expected)
                                row = receipt["commands"][-1]
                                self.assertEqual(row["exit_code"], 143 if raw == -15 else raw)
                                self.assertIn("subject_error" if cause == "source" else "record_error", row)

    return unittest.defaultTestLoader.loadTestsFromTestCase(SharedSdkIOTests)


def compiler_input_tests(owner):
    """Compose the original fixture without loading its common suite again."""
    class SdkCompilerInputTests(unittest.TestCase):
        @contextlib.contextmanager
        def subject(self, *, cause="", baseline_model=None, current_k8s=True):
            with tempfile.TemporaryDirectory() as temporary:
                helper = owner.SdkConsumerTests()
                root = helper.repository(Path(temporary))
                if baseline_model is not None:
                    helper.compiler_source(root, sdk_k8s=True, model=baseline_model, version="0.28.0")
                    helper.git(root, "add", ".")
                    helper.git(root, "commit", "--quiet", "-m", "private baseline compiler model")
                model = {"missing": None, "multiple": ["v1_32", "v1_33"], "invalid": "latest"}.get(cause, "v1_32")
                helper.compiler_source(root, sdk_k8s=current_k8s, model=model, version="0.28.0")
                (root / "tracked.txt").write_text("current compiler input subject\n")
                if cause == "unknown-dependency":
                    path = root / "crates/fixture-provider/Cargo.toml"
                    path.write_text(path.read_text() + 'unrecorded="1"\n')
                elif cause == "root-malformed":
                    (root / "Cargo.toml").write_text("[workspace\n")
                elif cause == "root-absent":
                    (root / "Cargo.toml").unlink()
                helper.git(root, "add", ".")
                helper.git(root, "commit", "--quiet", "-m", "private current compiler model")
                with mock.patch.dict(os.environ):
                    os.environ.pop("K8S_OPENAPI_ENABLED_VERSION", None)
                    yield helper, root

        @staticmethod
        def compiles(calls):
            return [command for command in calls if command[:2] in
                    (["cargo", "+1.96.0"], ["cargo", "semver-checks"])]

        def test_explicit_input_reaches_original_profiles_without_changing_ambient_or_baseline(self):
            # K1 model=v1_32 + supported unique registry identity + ambient
            # absent/equal => derive1.32 only in package/SemVer child values;
            # retain both original profile argv and SDK selection. K2 baseline
            # SDK normal/build closure has no K8s despite Platform v1_30 =>
            # baseline model=null/not_applicable, shared input inherited-unused.
            for mode in ("package", "semver"):
                for ambient in (None, "1.32"):
                    with self.subTest(mode=mode, ambient=ambient), self.subject() as (helper, root):
                        if ambient is not None:
                            os.environ["K8S_OPENAPI_ENABLED_VERSION"] = ambient
                        observations = []
                        status, calls = helper.execute(root, qualification=mode,
                            compiler_cause="valid", compiler_observations=observations)
                        self.assertEqual(status, 0)
                        self.assertEqual(len(self.compiles(calls)), 2)
                        self.assertEqual([row["compiler_input"] for row in observations], ["1.32", "1.32"])
                        self.assertEqual([row["ambient"] for row in observations], [ambient, ambient])
                        self.assertEqual(os.environ.get("K8S_OPENAPI_ENABLED_VERSION"), ambient)
                        path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                        receipt = json.loads(path.read_text())["compiler_inputs"]
                        self.assertEqual(receipt["environment"], {"K8S_OPENAPI_ENABLED_VERSION": "1.32"})
                        self.assertEqual(receipt["ambient"], "absent" if ambient is None else "matching")
                        self.assertEqual(receipt["current"]["model"]["feature"], "v1_32")
                        self.assertEqual(receipt["current"]["model"]["package"]["version"], "0.28.0")
                        if mode == "semver":
                            self.assertEqual(receipt["baseline"]["applicability"], "not_applicable")
                            self.assertIsNone(receipt["baseline"]["model"])
                            self.assertEqual(receipt["baseline"]["shared_environment"], "inherited_unused")

        def test_invalid_model_identity_graph_or_ambient_starts_no_compiler(self):
            # K3 empty/conflicting ambient; K4 absent/non-explicit/multiple or
            # unsupported model; K5 inconsistent/duplicate registry identity,
            # incomplete metadata or unresolved normal/build source edge =>
            # refusal before either compiler. No all-feature narrowing or SDK
            # library feature edit can substitute for this preflight evidence.
            causes = ("missing", "multiple", "invalid", "root-malformed", "unsupported", "identity",
                      "version", "duplicate", "incomplete", "unknown-dependency", "ambient-empty", "ambient-conflict")
            for mode in ("package", "semver"):
                for cause in causes:
                    with self.subTest(mode=mode, cause=cause), self.subject(cause=cause) as (helper, root):
                        if cause.startswith("ambient-"):
                            os.environ["K8S_OPENAPI_ENABLED_VERSION"] = "" if cause == "ambient-empty" else "1.33"
                        status, calls = helper.execute(root, qualification=mode, compiler_cause=cause)
                        self.assertEqual(status, 1)
                        self.assertEqual(self.compiles(calls), [])

        def test_applicable_sides_with_conflicting_models_refuse_without_overriding_baseline(self):
            # K6 baseline SDK actually reaches K8s and declaresv1_33 while the
            # current SDK declaresv1_32 => one shared compiler environment cannot
            # represent both; refuse before tool builds, do not borrow current.
            with self.subject(baseline_model="v1_33") as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (1, []))
            # The compatible branch requires both independent source/lock model
            # declarations, not a value copied from current. A current SDK that
            # no longer reaches K8s also cannot silently borrow baseline policy.
            with self.subject(baseline_model="v1_32") as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, len(self.compiles(calls))), (0, 2))
                path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                baseline = json.loads(path.read_text())["compiler_inputs"]["baseline"]
                self.assertEqual(baseline["model"]["value"], "1.32")
                self.assertEqual(baseline["shared_environment"], "model_used")
            with self.subject(baseline_model="v1_32", current_k8s=False) as (helper, root):
                status, calls = helper.execute(root, qualification="semver", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (1, []))

        def test_unreadable_inputs_and_post_child_faults_preserve_first_status(self):
            # K7 a Git source read failure retains its original status (75) or
            # IO refusal1 before any compiler. K8 child0/75/101/signal143 crossed
            # with post-source/log/journal uncertainty =>0 becomes1, original
            # nonzero wins and no second profile starts. Existing record/first
            # status owner, not the compiler-input policy, owns these outcomes.
            for error, expected in ((OSError("compiler input unreadable"), 1),
                                    (subprocess.CalledProcessError(75, ["git", "show"]), 75)):
                with self.subTest(read=expected), self.subject() as (helper, root):
                    original = owner.formal_sources.git_output
                    def read(subject, *arguments):
                        if arguments[0] == "show" and arguments[-1].endswith(":Cargo.toml"):
                            raise error
                        return original(subject, *arguments)
                    with mock.patch.object(owner.formal_sources, "git_output", side_effect=read):
                        status, calls = helper.execute(root, qualification="package", compiler_cause="valid")
                    self.assertEqual((status, self.compiles(calls)), (expected, []))
            with self.subject(cause="root-absent") as (helper, root):
                status, calls = helper.execute(root, qualification="package", compiler_cause="valid")
                self.assertEqual((status, self.compiles(calls)), (128, []))
            for raw in (0, 75, 101, -15):
                for cause in ("source", "log", "journal"):
                    with self.subTest(raw=raw, cause=cause), self.subject() as (helper, root):
                        arguments = {"mutation": "working"} if cause == "source" else {"fault": "output"} if cause == "log" else {}
                        with contextlib.ExitStack() as stack:
                            if cause == "journal":
                                stack.enter_context(mock.patch.object(owner, "retain_sdk_result", side_effect=OSError("compiler input receipt refusal")))
                            status, calls = helper.execute(root, qualification="package", compiler_cause="valid",
                                test_status=raw, phase="package", **arguments)
                        self.assertEqual(status, 143 if raw == -15 else raw or 1)
                        self.assertEqual(len(self.compiles(calls)), 1)

        def test_bare_and_original_artifact_modes_do_not_acquire_the_compiler_policy(self):
            # K9 the ordinary consumer and already-owned default artifact mode
            # retain their original behavior even with conflicting ambient input;
            # this package/SemVer policy adds neither a new prerequisite nor a
            # global sanitizer mutation to those existing paths.
            for mode in ("", "artifact"):
                with self.subTest(mode=mode), self.subject() as (helper, root):
                    os.environ["K8S_OPENAPI_ENABLED_VERSION"] = "conflicting-input"
                    status, _ = helper.execute(root, qualification=mode)
                    self.assertEqual(status, 0)
                    self.assertEqual(os.environ["K8S_OPENAPI_ENABLED_VERSION"], "conflicting-input")
                    if mode:
                        path = next((root / "target/test-timings").glob("sdk-qualification-*/commands.json"))
                        self.assertNotIn("compiler_inputs", json.loads(path.read_text()))

    return unittest.defaultTestLoader.loadTestsFromTestCase(SdkCompilerInputTests)
