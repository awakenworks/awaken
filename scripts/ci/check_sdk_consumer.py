#!/usr/bin/env python3
"""Compile and execute the independent SDK consumer without workspace features."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from generate_hooks import glob_matches

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "examples/sdk-consumer/Cargo.toml"
EXPECTED = {
    "single_turn_exposes_identity_state_and_committed_output",
    "multiple_turns_share_one_store_and_thread",
    "one_typed_tool_definition_supplies_schema_and_execution",
    "streaming_delivers_progress_and_commits_complete_unicode_output",
    "cancelled_input_never_enters_the_model",
    "asynchronous_approval_resumes_the_committed_ticket",
    "filesystem_restart_reads_receipt_and_replays_terminal_without_a_model",
}


def admission_errors(owner: str, gate: str) -> list[str]:
    errors = []
    trigger = re.search(r'glob: "([^"]+)"', owner)
    for path in ("examples/sdk-consumer/Cargo.toml", "examples/sdk-consumer/Cargo.lock",
                 "examples/sdk-consumer/src/main.rs", "examples/sdk-consumer/tests/tasks.rs",
                 "scripts/ci/check_sdk_consumer.py"):
        if trigger is None or not glob_matches(trigger.group(1), path):
            errors.append(f"Rust pre-commit must include the independent SDK consumer input: {path}")
    if gate.count("python3 scripts/ci/check_sdk_consumer.py") != 1:
        errors.append("Rust gate must execute the independent SDK consumer exactly once")
    return errors


def validate_task_evidence(output: str) -> None:
    executed = re.findall(r"^test ([a-z_]+) \.\.\. ok$", output, re.MULTILINE)
    summary = f"{len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    if set(executed) != EXPECTED or len(executed) != len(EXPECTED) or summary not in output:
        raise RuntimeError(f"SDK consumer task evidence is incomplete: expected={sorted(EXPECTED)}, executed={sorted(executed)}")


def self_test() -> None:
    owner = 'glob: "{examples/sdk-consumer/**,scripts/ci/check_sdk_consumer.py}"'
    gate = "python3 scripts/ci/check_sdk_consumer.py"
    assert not admission_errors(owner, gate)
    assert admission_errors(owner.replace("examples/sdk-consumer/**,", ""), gate)
    assert admission_errors(owner, "true")
    assert admission_errors(owner, gate + "\n" + gate)
    passing = "\n".join(f"test {name} ... ok" for name in sorted(EXPECTED))
    summary = "\ntest result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    validate_task_evidence(passing + summary)
    for invalid in (
        passing.split("\n", 1)[1] + summary,
        passing + "\n" + passing.split("\n", 1)[0] + summary,
        passing + summary.replace("0 ignored", "1 ignored"),
        passing + summary.replace("0 filtered out", "1 filtered out"),
        passing.replace(" ... ok", " ... FAILED", 1) + summary,
    ):
        try:
            validate_task_evidence(invalid)
        except RuntimeError:
            continue
        raise AssertionError("incomplete consumer execution evidence was accepted")


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test()
    if args.self_test:
        print("SDK consumer self-test: admission paths and exact task execution are enforced")
        return 0
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    metadata = subprocess.run(["cargo", "metadata", "--manifest-path", str(MANIFEST), "--no-deps", "--format-version", "1", "--locked"], cwd=ROOT, env=env, capture_output=True, text=True)
    if metadata.returncode:
        sys.stderr.write(metadata.stderr)
        return metadata.returncode
    data = json.loads(metadata.stdout)
    if Path(data["workspace_root"]).resolve() != MANIFEST.parent.resolve() or len(data["workspace_members"]) != 1:
        raise RuntimeError("SDK consumer must be its own one-member workspace")
    packages = data["packages"]
    if any(dependency["name"] == "awaken-runtime-examples" for package in packages for dependency in package["dependencies"]):
        raise RuntimeError("consumer cannot depend on the teaching fixture prelude")
    result = subprocess.run(["cargo", "test", "--manifest-path", str(MANIFEST), "--locked", "--test", "tasks", "--", "--test-threads=1"], cwd=ROOT, env=env, capture_output=True, text=True)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode:
        return result.returncode
    validate_task_evidence(result.stdout)
    print("SDK consumer: all seven task cases passed in an independent minimal-feature workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
