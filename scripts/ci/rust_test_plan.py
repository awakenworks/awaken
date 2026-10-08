#!/usr/bin/env python3
"""Derive local affected tests from the actual Cargo graph; never bless a full gate."""
from __future__ import annotations

import argparse
from collections import deque
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
PROFILE = ROOT / ".config/rust-tests.toml"
INPUT_GLOBS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates/**", ".config/**", "scripts/ci/**")


def workspace_features() -> str:
    features = tomllib.loads(PROFILE.read_text(encoding="utf-8"))["workspace_features"]
    if not isinstance(features, list) or not features or any(not isinstance(f, str) or not f for f in features) or len(set(features)) != len(features):
        raise ValueError("workspace_features must be a nonempty unique string list")
    return ",".join(features)


def affected_packages(metadata: dict, changed: list[str], root: Path = ROOT) -> tuple[list[str], str]:
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    owners = sorted(((Path(p["manifest_path"]).parent.relative_to(root).as_posix(), identity) for identity, p in packages.items()), reverse=True)
    selected: set[str] = set()
    for path in changed:
        if path.startswith("docs/") and path.endswith(".md"):
            continue
        owner = next((identity for directory, identity in owners if path == directory or path.startswith(directory + "/")), None)
        if owner is None:
            return sorted(p["name"] for p in packages.values()), "global or unowned input: " + path
        selected.add(owner)
    if not selected:
        return [], "no changed Rust ownership boundary; this is not a full-gate result"
    reverse: dict[str, set[str]] = {identity: set() for identity in members}
    if metadata.get("resolve") is None:
        raise ValueError("Cargo's resolved feature/dependency graph is required")
    for node in metadata["resolve"]["nodes"]:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    reverse[dependency["pkg"]].add(node["id"])
    pending = deque(selected)
    while pending:
        for consumer in reverse[pending.popleft()] - selected:
            selected.add(consumer)
            pending.append(consumer)
    return sorted(packages[identity]["name"] for identity in selected), "changed owners and transitive consumers, including test dependencies"


def git_output(*arguments: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(ROOT), *arguments])


def source_digest(revision: bytes, patch: bytes, untracked: dict[str, bytes]) -> str:
    digest = hashlib.sha256()
    for value in (revision, patch, *(part for name, data in sorted(untracked.items()) for part in (name.encode(), data))):
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)
    return digest.hexdigest()


def source_snapshot() -> tuple[str, bytes]:
    patch = git_output("diff", "--binary", "HEAD", "--")
    paths = git_output("ls-files", "--others", "--exclude-standard", "-z").decode().split("\0")
    untracked = {name: (ROOT / name).read_bytes() for name in paths if name}
    return source_digest(git_output("rev-parse", "HEAD"), patch, untracked), patch


class PlanTests(unittest.TestCase):
    def test_receipt_identity_includes_untracked_bytes_and_boundaries(self) -> None:
        expected = source_digest(b"revision", b"patch", {"a": b"bc"})
        for revision, patch, files in ((b"other", b"patch", {"a": b"bc"}), (b"revision", b"changed", {"a": b"bc"}), (b"revision", b"patch", {"a": b"different"}), (b"revision", b"patch", {"ab": b"c"})):
            self.assertNotEqual(expected, source_digest(revision, patch, files))
        self.assertEqual(source_digest(b"r", b"p", {"b": b"2", "a": b"1"}), source_digest(b"r", b"p", {"a": b"1", "b": b"2"}))
    def test_shared_profile_names_declared_workspace_features(self) -> None:
        manifests = {tomllib.loads(path.read_text(encoding="utf-8"))["package"]["name"]: path for path in ROOT.glob("crates/*/*/Cargo.toml")}
        for feature in workspace_features().split(","):
            package, name = feature.split("/", 1)
            self.assertIn(package, manifests)
            self.assertIn(name, tomllib.loads(manifests[package].read_text(encoding="utf-8")).get("features", {}))

    def test_ownership_reverse_consumers_and_global_fallback(self) -> None:
        # A1 leaf => leaf+consumer+composition; A2 unrelated owner excluded.
        # A3 manifests/CI/unowned/deleted package => full workspace. A4 only
        # docs/no changes => explicit no-work, never an invented green receipt.
        names = ("kernel", "application", "cli", "other")
        metadata = {
            "workspace_members": list(names),
            "packages": [{"id": n, "name": n, "manifest_path": str(ROOT / "crates" / n / "Cargo.toml")} for n in names],
            "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in deps]} for n, deps in (("kernel", ()), ("application", ("kernel",)), ("cli", ("application",)), ("other", ()))]},
        }
        for paths, expected in ((["crates/kernel/src/lib.rs"], ["application", "cli", "kernel"]), (["crates/cli/tests/new.rs"], ["cli"]), (["crates/other/Cargo.toml"], ["other"]), (["docs/design.md"], []), ([], [])):
            self.assertEqual(affected_packages(metadata, paths)[0], expected)
        for path in ("Cargo.lock", "Cargo.toml", ".config/rust-tests.toml", "scripts/ci/check-rust.sh", "crates/deleted/src/lib.rs", "e2e/scenario.ts"):
            self.assertEqual(affected_packages(metadata, [path])[0], sorted(names))
        metadata["resolve"] = None
        with self.assertRaises(ValueError):
            affected_packages(metadata, ["crates/kernel/src/lib.rs"])


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--features", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--source-digest", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    if args.features or args.self_test or args.source_digest:
        if args.base or args.run or sum((args.features, args.self_test, args.source_digest)) != 1:
            parser.error("--features, --source-digest and --self-test are standalone modes")
        if args.source_digest:
            print(source_snapshot()[0])
            return 0
        if args.features:
            print(workspace_features())
            return 0
        return int(not unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PlanTests)).wasSuccessful())
    if not args.base:
        parser.error("--base is required for an affected plan")
    base = git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()
    changed = sorted(set(git_output("diff", "--name-only", "-z", base, "--").decode().split("\0") + git_output("ls-files", "--others", "--exclude-standard", "-z").decode().split("\0")) - {""})
    features = workspace_features()
    environment = dict(os.environ)
    environment.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    environment["CARGO_BUILD_BUILD_DIR"] = environment["CARGO_TARGET_DIR"]
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1", "--features", features], cwd=ROOT, env=environment))
    selected, reason = affected_packages(metadata, changed)
    command = ["cargo", "nextest", "run", "--workspace", "--locked", "--features", features]
    if selected:
        command += ["-E", " | ".join(f"package(={name})" for name in selected)]
    plan = {"schema_version": 1, "kind": "local-affected", "revision": git_output("rev-parse", "HEAD").decode().strip(), "base": base, "changed": changed, "selected_packages": selected, "reason": reason, "command": command if selected else [], "full_gate": False}
    print(json.dumps(plan, indent=2), flush=True)
    if not args.run or not selected:
        return 0
    directory = Path(environment["CARGO_TARGET_DIR"]) / "test-timings"
    directory.mkdir(parents=True, exist_ok=True)
    receipt = directory / f"affected-{time.time_ns()}.json"
    fingerprint, patch = source_snapshot()
    plan["source_digest"] = fingerprint
    receipt.with_suffix(".patch").write_bytes(patch)
    started = time.monotonic()
    status = subprocess.run(command, cwd=ROOT, env=environment).returncode
    unchanged = source_snapshot()[0] == fingerprint
    if not unchanged:
        print("Source changed during affected execution; result cannot qualify this tree", file=sys.stderr)
        status = status or 1
    plan.update(exit_code=status, source_unchanged=unchanged, duration_seconds=time.monotonic() - started)
    receipt.write_text(json.dumps(plan, indent=2), encoding="utf-8")
    print(f"Affected execution evidence: {receipt}")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
