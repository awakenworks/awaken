#!/usr/bin/env python3
"""Fence the certificate-data exception for the immutable 0.26 compatibility shim.

webpki-roots 0.26.11 contains only `pub use parent::TLS_SERVER_ROOTS`.
Its fixed crates.io checksum attests that implementation. RUSTSEC-2026-0381
also matches its old version number, although the data comes from v1. The
exception is sound only when every actual root-data package is patched and
the original shim points directly to that single registry package.
"""
from __future__ import annotations

import argparse
import copy
from contextlib import redirect_stderr, redirect_stdout
import io
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
INPUT_GLOBS = ("Cargo.lock", "deny.toml", "scripts/ci/check_trust_roots.py")
ADVISORY = "RUSTSEC-2026-0381"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
SHIM_CHECKSUM = "521bc38abb08001b01866da9f51eb7c5d647a19260e00054a8c7fd5f9e57f7a9"


def violations(lock: dict, policy: dict) -> list[str]:
    roots = [p for p in lock.get("package", []) if p.get("name") == "webpki-roots"]
    shims = [p for p in roots if p.get("version") == "0.26.11"]
    data = [p for p in roots if p not in shims]
    ignores = [p for p in policy.get("advisories", {}).get("ignore", [])
               if (p.get("id") if isinstance(p, dict) else p) == ADVISORY]
    errors = []
    if shims:
        if (len(ignores) != 1 or not isinstance(ignores[0], dict)
                or not isinstance(ignores[0].get("reason"), str)
                or len(ignores[0]["reason"].strip()) < 30):
            errors.append("The compatibility exception requires one documented reason")
        if (len(shims) != 1 or shims[0].get("source") != REGISTRY
                or shims[0].get("checksum") != SHIM_CHECKSUM):
            errors.append("Only the original, checksum-pinned registry shim is admitted")
        if len(data) != 1 or shims[0].get("dependencies") != [
                "webpki-roots " + (data[0].get("version", "") if len(data) == 1 else "")]:
            errors.append("The shim must directly forward to exactly one real root-data package")
    elif ignores:
        errors.append("Remove the stale compatibility exception when the last shim is gone")
    for package in data:
        version = package.get("version", "")
        parsed = tuple(map(int, version.split("."))) if re.fullmatch(r"\d+\.\d+\.\d+", version) else ()
        if package.get("source") != REGISTRY or not ((1, 0, 10) <= parsed < (2, 0, 0)):
            errors.append("Actual root data must use registry webpki-roots >=1.0.10 and <2")
    if shims and not data:
        errors.append("The compatibility exception cannot hide missing certificate data")
    return errors


def audit_lockfile(lock: dict) -> int:
    # Older cached audit databases can retain an old local HEAD after fetch.
    # Use an owned fresh clone of official HEAD, record its revision, and audit
    # precisely those bytes. Never rewrite a developer's shared advisory cache.
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_GLOBAL=os.devnull,
                       GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
    with tempfile.TemporaryDirectory(prefix="awaken-audit-") as directory:
        database = str(Path(directory) / "advisory-db")
        try:
            result = subprocess.run(["git", "-c", "credential.helper=", "clone", "--quiet", "--depth=1",
                                     "https://github.com/RustSec/advisory-db.git", database],
                                    cwd=directory, env=environment, timeout=180)
        except subprocess.TimeoutExpired:
            return 124
        if result.returncode:
            return result.returncode
        revision = subprocess.check_output(["git", "-C", database, "rev-parse", "HEAD"],
                                           env=environment, text=True).strip()
        print("Official advisory database revision: " + revision, flush=True)
        shim = any(p.get("name") == "webpki-roots" and p.get("version") == "0.26.11"
                   for p in lock.get("package", []))
        return subprocess.run(["cargo", "audit", "--deny", "warnings", "--db", database, "--no-fetch"]
                              + (["--ignore", ADVISORY] if shim else []), cwd=ROOT).returncode


class TrustRootTests(unittest.TestCase):
    def fixture(self):
        return ({"package": [
            {"name": "webpki-roots", "version": "0.26.11", "source": REGISTRY,
             "checksum": SHIM_CHECKSUM, "dependencies": ["webpki-roots 1.0.10"]},
            {"name": "webpki-roots", "version": "1.0.10", "source": REGISTRY},
        ]}, {"advisories": {"ignore": [{"id": ADVISORY,
                                       "reason": "Immutable compatibility re-export; patched v1 owns all data"}]}})

    def test_original_shim_and_patched_data_are_admitted(self):
        self.assertEqual(violations(*self.fixture()), [])

    def test_modern_only_graph_requires_no_exception(self):
        lock, _ = self.fixture()
        self.assertEqual(violations({"package": lock["package"][1:]}, {}), [])
        self.assertEqual(violations({"package": []}, {}), [])

    def test_changed_checksum_source_or_forwarding_edge_is_rejected(self):
        for field, value in (("checksum", "0" * 64), ("source", "path+local"),
                             ("dependencies", []), ("dependencies", ["webpki-roots 1.0.9"])):
            lock, policy = self.fixture()
            lock["package"][0][field] = value
            self.assertTrue(violations(lock, policy), field)

    def test_unpatched_data_cannot_hide_behind_the_exception(self):
        for version in ("1.0.8", "1.0.9", "0.26.10", "1.0.10-alpha.1", "2.0.0"):
            lock, policy = self.fixture()
            lock["package"][1]["version"] = version
            lock["package"][0]["dependencies"] = ["webpki-roots " + version]
            self.assertTrue(violations(lock, policy), version)

    def test_missing_duplicate_or_replaced_data_is_rejected(self):
        for change in ("missing", "duplicate", "replaced"):
            lock, policy = self.fixture()
            if change == "missing":
                lock["package"].pop()
            elif change == "duplicate":
                lock["package"].append(copy.deepcopy(lock["package"][1]))
            else:
                lock["package"][1]["source"] = "git+https://example.invalid/data"
            self.assertTrue(violations(lock, policy), change)

    def test_absent_undocumented_duplicate_or_stale_exception_is_rejected(self):
        lock, policy = self.fixture()
        self.assertTrue(violations(lock, {}))
        for ignore in ([ADVISORY], [{"id": ADVISORY, "reason": ""}],
                       policy["advisories"]["ignore"] * 2):
            self.assertTrue(violations(lock, {"advisories": {"ignore": ignore}}))
        self.assertTrue(violations({"package": lock["package"][1:]}, policy))

    def test_new_patched_data_remains_supported(self):
        lock, policy = self.fixture()
        lock["package"][1]["version"] = "1.0.11"
        lock["package"][0]["dependencies"] = ["webpki-roots 1.0.11"]
        self.assertEqual(violations(lock, policy), [])

    def test_audit_cannot_start_with_unpatched_or_replaced_data(self):
        for field, value in (("version", "1.0.9"), ("source", "path+local")):
            lock, policy = self.fixture()
            lock["package"][1][field] = value
            with patch.object(Path, "read_text", return_value="fixture"), \
                 patch.object(tomllib, "loads", side_effect=[lock, policy]), \
                 patch.object(subprocess, "run") as run, redirect_stderr(io.StringIO()):
                self.assertEqual(main(["--audit"]), 1)
                run.assert_not_called()

    def test_audit_passes_only_the_conditionally_justified_advisory(self):
        for shim in (True, False):
            lock, policy = self.fixture()
            if not shim:
                lock["package"].pop(0)
                policy = {}
            with patch.object(Path, "read_text", return_value="fixture"), \
                 patch.object(tomllib, "loads", side_effect=[lock, policy]), \
                 patch.object(subprocess, "run", side_effect=lambda args, **kw:
                              subprocess.CompletedProcess(args, 0 if args[0] == "git" else 23)) as run, \
                 patch.object(subprocess, "check_output", return_value="1" * 40), redirect_stdout(io.StringIO()):
                self.assertEqual(main(["--audit"]), 23)
                args = run.call_args.args[0]
                self.assertEqual(args[:5], ["cargo", "audit", "--deny", "warnings", "--db"])
                self.assertEqual(args[6:], ["--no-fetch"] + (["--ignore", ADVISORY] if shim else []))
                self.assertEqual(run.call_args_list[0].kwargs["env"]["GIT_CONFIG_NOSYSTEM"], "1")

    def test_advisory_fetch_failure_or_timeout_never_starts_audit(self):
        for outcome, expected in ((subprocess.CompletedProcess([], 75), 75),
                                  (subprocess.TimeoutExpired("git", 180), 124)):
            with patch.object(subprocess, "run", **({"side_effect": outcome} if isinstance(outcome, Exception)
                                                   else {"return_value": outcome})) as run, \
                 patch.object(subprocess, "check_output") as resolve:
                self.assertEqual(audit_lockfile(self.fixture()[0]), expected)
                self.assertEqual(run.call_count, 1)
                resolve.assert_not_called()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true")
    modes.add_argument("--audit", action="store_true", help="audit the complete lockfile after validating the exception")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(TrustRootTests)).wasSuccessful() else 1
    try:
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
        errors = violations(lock, tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8")))
    except (OSError, ValueError) as error:
        errors = [str(error)]
    for error in errors:
        print("ERROR: " + error, file=sys.stderr)
    if not errors:
        print("Trust-root compatibility: immutable shim and patched data verified")
        if args.audit:
            return audit_lockfile(lock)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
