"""Record completion of the existing formal gate for signed release provenance.

This is a CI execution receipt, not an independent proof certificate. Its
authenticity comes from the existing trusted quality workflow and release
attestation. There is no second proof catalog: check_formal.sh remains the runner.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[2]


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def digest_bytes(value: bytes) -> str:
    return "sha256:" + hashlib.sha256(value).hexdigest()


def digest_file(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def proof_toolchain(repository: Path) -> dict:
    rust = tomllib.loads((repository / "rust-toolchain.toml").read_text())
    bootstrap = (repository / "scripts/ci/bootstrap_kani.sh").read_text()
    gate = (repository / "scripts/ci/check_formal.sh").read_text()

    def constant(source: str, name: str) -> str:
        match = re.search(rf'readonly {name}="([^"]+)"', source)
        if match is None:
            raise ValueError(f"missing pinned {name}")
        return match[1]

    return {
        "rust_toolchain": rust["toolchain"]["channel"],
        "kani_source_revision": constant(bootstrap, "kani_revision"),
        "kani_nightly": constant(bootstrap, "kani_nightly"),
        "kani_backend": constant(bootstrap, "kani_backend_version"),
        "tlc": constant(gate, "tlc_version"),
        "tlaps": constant(gate, "tlapm_version"),
    }


def source_snapshot(repository: Path) -> dict:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", *args], cwd=repository)
    # Git normalizes platform line endings. The full tree binds transitive
    # modules, models/configs, dependency locks, generator and runner sources.
    # Dirty runs are useful diagnostics, but cannot qualify a release.
    status = git("status", "--porcelain=v1", "--untracked-files=all")
    return {
        "commit": git("rev-parse", "HEAD").decode().strip(),
        "tree": git("rev-parse", "HEAD^{tree}").decode().strip(),
        "clean": not status,
        "working_diff_digest": digest_bytes(git("diff", "--binary", "HEAD")),
        "status_digest": digest_bytes(status),
    }


def runner_identity(repository: Path) -> dict:
    path = "scripts/ci/check_formal.sh"
    # The quality runner and Windows packaging may check out different EOLs.
    # Bind the exact Git blob; source_snapshot separately rejects real edits.
    blob = subprocess.check_output(["git", "show", "HEAD:" + path], cwd=repository)
    return {"path": path, "digest": digest_bytes(blob)}


def begin_record(repository: Path, strict: bool, now: int) -> dict:
    return {
        "schema_version": 1, "run_id": str(uuid.uuid4()), "status": "running",
        "strict": strict, "started_at_unix": now,
        "source": source_snapshot(repository),
        "proof_toolchain": proof_toolchain(repository),
        "runner": runner_identity(repository),
    }


def finish_record(record: dict, repository: Path, missing_tools: bool, now: int) -> dict:
    if record.get("status") != "running" or record.get("schema_version") != 1:
        raise ValueError("formal evidence must finish one running invocation")
    if record.get("strict") is not True or missing_tools:
        raise ValueError("only a complete strict formal run can produce success evidence")
    if type(now) is not int or now < record["started_at_unix"]:
        raise ValueError("formal evidence completion precedes its start")
    if record["source"] != source_snapshot(repository):
        raise ValueError("formal evidence source changed during execution")
    if record["proof_toolchain"] != proof_toolchain(repository):
        raise ValueError("formal evidence toolchain contract changed during execution")
    if record["runner"] != runner_identity(repository):
        raise ValueError("formal evidence runner changed during execution")
    return dict(record, status="passed", finished_at_unix=now)


def validate_record(record: dict, repository: Path, now: int | None = None) -> None:
    if record.get("schema_version") != 1 or record.get("status") != "passed" or record.get("strict") is not True:
        raise ValueError("formal evidence is not a completed strict run")
    started, finished = record.get("started_at_unix"), record.get("finished_at_unix")
    clock = int(time.time()) if now is None else now
    if type(started) is not int or type(finished) is not int or not 0 <= started <= finished <= clock:
        raise ValueError("formal evidence has invalid execution times")
    if not isinstance(record.get("run_id"), str) or not record["run_id"]:
        raise ValueError("formal evidence has no invocation identity")
    expected_source = source_snapshot(repository)
    if not expected_source["clean"] or record.get("source") != expected_source:
        raise ValueError("formal evidence does not match the clean release source")
    if record.get("proof_toolchain") != proof_toolchain(repository):
        raise ValueError("formal evidence toolchain contract mismatch")
    if record.get("runner") != runner_identity(repository):
        raise ValueError("formal evidence runner mismatch")


def write_record(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Readers see the previous complete record or the new one, never torn JSON.
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        temporary.write(canonical_bytes(record))
        temporary.flush()
        os.fsync(temporary.fileno())
        name = temporary.name
    try:
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("begin", "finish"))
    parser.add_argument("--record", type=Path, required=True)
    parser.add_argument("--strict", choices=("0", "1"), default="0")
    parser.add_argument("--missing-tools", choices=("0", "1"), default="0")
    args = parser.parse_args()
    now = int(time.time())
    if args.action == "begin":
        record = begin_record(ROOT, args.strict == "1", now)
    else:
        record = finish_record(json.loads(args.record.read_text()), ROOT, args.missing_tools == "1", now)
    write_record(args.record, record)


if __name__ == "__main__":
    main()
