#!/usr/bin/env python3
"""Verify public exact Git revisions and a locked workspace fetch with empty caches.

This network tier is separate from the cheap dependency-source policy scan.
Each invocation uses new Git stores and CARGO_HOME; it cannot succeed from a
developer's cached unpublished commit, Git URL rewrite or Cargo source override.
No compilation is performed. Evidence survives both failures and successful runs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

import check_dependency_sources as policy


ROOT = Path(__file__).resolve().parents[2]


def workflow_errors(text: str) -> list[str]:
    match = re.search(r"(?ms)^  dependency_fetch:\n(.*?)(?=^  \S|\Z)", text)
    if not match:
        return ["quality workflow must run the cold dependency tier"]
    job = match.group(1)
    command = "run: python scripts/ci/check_dependency_fetch.py --output target/test-timings/dependency-fetch"
    errors = []
    if "runner: [ubuntu-22.04, windows-2025]" not in job or job.count(command) != 1:
        errors.append("cold dependency fetch must run on both Linux and Windows")
    conditions = re.findall(r"(?m)^[ \t]*if:[ \t]*(.*)$", job)
    if (any(value in job for value in ("rust-cache", "actions/cache", "continue-on-error", "--git-only"))
            or conditions != ["always()"]):
        errors.append("cold dependency tier must not restore caches or suppress verification")
    for guard in ("python scripts/ci/check_trust_roots.py --self-test",
                  "python scripts/ci/check_trust_roots.py\n"):
        if guard not in job:
            errors.append("cold dependency tier must enforce the conditional trust-root policy")
    # tomllib is part of the checker runtime. Ubuntu 22.04's default Python
    # predates it; bootstrap the selected interpreter before every quality job consumes repository checkers.
    setup = "uses: actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1 # v6"
    for name in ("dependency_fetch", "trusted_host_portability", "full_quality", "performance_summary"):
        native = re.search(r"(?ms)^  " + name + r":\n(.*?)(?=^  \S|\Z)", text)
        body = native.group(1) if native else ""
        bootstrap = setup + '\n        with:\n          python-version: "3.12"'
        first_python = re.search(r"(?m)^.*(?:run: |^ +)python3? scripts/ci/", body)
        step = re.search(r"(?m)^      - " + re.escape(bootstrap) + r"\n(?:(?!^      - ).|\n)*", body)
        if (body.count(bootstrap) != 1 or first_python is None or step is None
                or re.search(r"(?m)^        (?:if|continue-on-error):", step.group(0))
                or body.index(bootstrap) > first_python.start()):
            errors.append(f"{name} must bootstrap the pinned Python before its first checker")
    for name in ("trusted_host_portability", "full_quality"):
        if not re.search(r"(?m)^  " + name + r":\n    needs: dependency_fetch\n", text):
            errors.append(f"{name} must wait for cold dependency verification")
    return errors



def native_workflow_errors(text: str) -> list[str]:
    """Cold dependency admission must precede every native service build/test."""
    command = "run: python scripts/ci/check_dependency_fetch.py --output target/test-timings/native-dependency-fetch"
    match = re.search(r"(?ms)^  native_services:\n(.*?)(?=^  \S|\Z)", text)
    body = match.group(1) if match else ""
    # A single step owns admission. Its failure must block all following work;
    # the always-upload step only retains evidence and never grants admission.
    steps = re.split(r"(?m)^      - ", body)
    selected = [step for step in steps if "check_dependency_fetch.py" in step]
    if len(selected) != 1 or selected[0].count(command) != 1:
        return ["native services must require one complete cold dependency fetch"]
    step = selected[0]
    first_native = re.search(r"(?m)^.*run: python scripts/ci/(?:test_native|check_native_services|install_windows_services)", body)
    if (re.search(r"(?m)^    (?:if|continue-on-error):", body)
            or re.search(r"(?m)^        (?:if|continue-on-error):", step)
            or "continue-on-error:" in body or "--git-only" in step
            or any(value in body for value in ("rust-cache", "actions/cache"))
            or body.count("runs-on: windows-2025") != 1
            or first_native is None or body.index(command) > first_native.start()):
        return ["native cold dependency admission must be unconditional and precede uncached Windows verification"]
    return []


def clean_environment() -> dict[str, str]:
    return {**{key: value for key, value in os.environ.items()
               if not key.startswith(("GIT_", "CARGO_"))},
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0"}


def tracked_paths(root: Path) -> list[str]:
    result = subprocess.run(["git", "-C", str(root), "ls-files", "-z"],
                            env=clean_environment(), capture_output=True, check=True)
    return sorted(set(path.decode("utf-8") for path in result.stdout.split(b"\0") if path))


def git_inputs(root: Path, paths: list[str]) -> list[tuple[str, str]]:
    inputs = set()
    errors = []
    for relative in paths:
        path = root / relative
        if not path.is_file():
            continue
        if path.name == "Cargo.toml":
            data = policy.load_toml(path)
            violations = policy.validate_manifest_data(data, relative)
            errors.extend(violations)
            if violations:
                continue
            for _, spec in policy.dependency_specs(data):
                repository, error = policy.normalized_repository(spec["git"])
                if not error and isinstance(spec.get("rev"), str):
                    inputs.add((repository, spec["rev"]))
        elif path.name == "Cargo.lock":
            data = policy.load_toml(path)
            violations = policy.validate_lock_data(data, relative)
            errors.extend(violations)
            if violations:
                continue
            for package in data.get("package", []):
                if not isinstance(package, dict):
                    continue
                source = package.get("source", "")
                if isinstance(source, str) and source.startswith("git+") and "replace" not in package:
                    from urllib.parse import urlsplit
                    parsed = urlsplit(source.removeprefix("git+"))
                    repository, error = policy.normalized_repository(
                        f"{parsed.scheme}://{parsed.netloc}{parsed.path}")
                    if not error:
                        inputs.add((repository, parsed.fragment))
    if errors:
        raise ValueError("\n".join(errors))
    if not inputs:
        raise ValueError("No tracked Git dependency inputs were found")
    return sorted(inputs)


def run(argv: list[str], cwd: Path, environment: dict[str, str], log: Path,
        *, timeout: int = 180) -> dict[str, object]:
    started = time.monotonic()
    with log.open("wb") as stream:
        try:
            result = subprocess.run(argv, cwd=cwd, env=environment, stdout=stream,
                                    stderr=subprocess.STDOUT, timeout=timeout)
            code = result.returncode
        except subprocess.TimeoutExpired:
            stream.write(b"\nFAILED: dependency verification timed out\n")
            code = 124
    return {"command": argv, "exit_code": code, "seconds": round(time.monotonic()-started, 3),
            "log": str(log)}


def fetch_revision(repository: str, revision: str, store: Path, output: Path,
                   environment: dict[str, str]) -> dict[str, object]:
    store.mkdir()
    rows = [run(["git", "init", "--bare", str(store)], store.parent, environment, output / "init.log")]
    if rows[-1]["exit_code"] == 0:
        rows.append(run(["git", "-c", "credential.helper=", "-c", "http.extraHeader=",
                         "--git-dir", str(store), "fetch", "--no-tags", "--depth=1", repository, revision],
                        store.parent, environment, output / "fetch.log"))
    resolved = ""
    if rows[-1]["exit_code"] == 0:
        row = run(["git", "--git-dir", str(store), "rev-parse", "--verify", "FETCH_HEAD^{commit}"],
                  store.parent, environment, output / "resolved.log")
        rows.append(row)
        resolved = (output / "resolved.log").read_text(encoding="utf-8").strip()
    return {"repository": repository, "requested_revision": revision, "resolved_revision": resolved,
            "stages": rows, "passed": all(row["exit_code"] == 0 for row in rows) and resolved == revision}


def snapshot(root: Path, paths: list[str], destination: Path) -> str:
    digest = hashlib.sha256()
    destination.mkdir()
    for relative in paths:
        source, target = root / relative, destination / relative
        if not source.exists():
            continue
        if not source.is_file() or source.is_symlink():
            raise ValueError(f"Snapshot requires a regular tracked file: {relative}")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        digest.update(relative.encode("utf-8") + b"\0" + hashlib.sha256(target.read_bytes()).digest())
    return digest.hexdigest()


def source_identity(root: Path) -> dict[str, str]:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", "-C", str(root), *args], env=clean_environment())
    return {"revision": git("rev-parse", "HEAD").decode().strip(),
            "tracked_patch_sha256": hashlib.sha256(git("diff", "HEAD", "--binary")).hexdigest()}


def verify(root: Path, output: Path, *, git_only: bool = False) -> dict[str, object]:
    paths = tracked_paths(root)
    inputs = git_inputs(root, paths)
    output.mkdir(parents=True, exist_ok=True)
    record = {"passed": False, "mode": "public-git-only" if git_only else "cold-workspace-fetch",
              "source": source_identity(root), "platform": platform.platform(),
              "git_dependencies": [], "cargo_fetch": None,
              "isolation": {"empty_git_object_stores": True, "empty_cargo_home": not git_only,
                            "global_and_system_git_config": "disabled",
                            "inherited_git_and_cargo_overrides": "removed"}}
    try:
        with tempfile.TemporaryDirectory(prefix="cold-dependency-") as directory:
            scratch = Path(directory).resolve()
            # Cargo also searches ancestors of cwd; refuse inherited configuration.
            for ancestor in (scratch, *scratch.parents):
                for name in ("config", "config.toml"):
                    if (ancestor / ".cargo" / name).exists():
                        raise ValueError(f"Cold cwd has an ancestor Cargo config: {ancestor}")
            environment = clean_environment()
            for index, (repository, revision) in enumerate(inputs):
                logs = output / f"git-{index}"
                logs.mkdir(exist_ok=True)
                result = fetch_revision(repository, revision, scratch / f"git-{index}", logs, environment)
                record["git_dependencies"].append(result)
                print(json.dumps({"repository": repository, "revision": revision,
                                  "passed": result["passed"]}), flush=True)
            if not all(row["passed"] for row in record["git_dependencies"]):
                return record
            if not git_only:
                checkout = scratch / "source"
                record["snapshot_sha256"] = snapshot(root, paths, checkout)
                home = scratch / "cargo-home"
                home.mkdir()
                environment.update(CARGO_HOME=str(home), CARGO_NET_GIT_FETCH_WITH_CLI="true")
                record["cargo_home_initial_entries"] = list(home.iterdir())
                record["cargo_version"] = run(["cargo", "--version", "--verbose"], checkout, environment,
                                               output / "cargo-version.log")
                if record["cargo_version"]["exit_code"] != 0:
                    return record
                record["cargo_fetch"] = run(["cargo", "fetch", "--locked"], checkout, environment,
                                             output / "cargo-fetch.log", timeout=1200)
                if record["cargo_fetch"]["exit_code"] != 0:
                    return record
            record["passed"] = True
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        record["error"] = str(error)
    finally:
        if source_identity(root) != record["source"]:
            record["passed"] = False
            record["error"] = "Tracked source changed during cold dependency verification"
        (output / "result.json").write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    return record


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--git-only", action="store_true", help="verify fresh public Git objects without Cargo fetch")
    parser.add_argument("--output", type=Path, default=ROOT / "target/test-timings/dependency-fetch")
    args = parser.parse_args(argv)
    if args.self_test:
        import unittest
        from _dependency_fetch_tests import DependencyFetchTests
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(DependencyFetchTests)).wasSuccessful() else 1
    try:
        result = verify(ROOT, args.output.resolve(), git_only=args.git_only)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Cold dependency fetch FAILED: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"passed": result["passed"], "evidence": str(args.output / "result.json")}))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
