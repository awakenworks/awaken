"""Record completion of the existing formal gate for signed release provenance.

This is a CI execution receipt, not an independent proof certificate. Its
authenticity comes from the existing trusted quality workflow and release
attestation. There is no second proof catalog: check_formal.sh remains the runner.

A receipt names the plan it ran: the tier (``full`` or ``pr``), the affected
base a ``pr`` run was narrowed to, and the digest of the derived Kani plan
(``formal_plan``). Only a ``full``, un-narrowed receipt whose digest is the
release source's own full plan qualifies a release.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import tempfile
import time
import tomllib
import uuid

import formal_plan
import formal_sources

ROOT = Path(__file__).resolve().parents[2]
# Schema 2 adds the plan selection and its digest to schema 1.
RUN_SCHEMA_VERSION = 2
FAILURE_KINDS = ("execution-failed", "resource-bound", "memory-bound")
# The only selection a release accepts: every harness, nothing narrowed.
FULL_SELECTION = {"tier": "full", "affected_base": None}


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
        "kani_source_patch_sha256": constant(bootstrap, "kani_patch_sha256"),
        "kani_nightly": constant(bootstrap, "kani_nightly"),
        "kani_backend": constant(bootstrap, "kani_backend_version"),
        "cbmc_source_revision": constant(bootstrap, "cbmc_revision"),
        "cbmc_source_patch_sha256": constant(bootstrap, "cbmc_patch_sha256"),
        "cbmc_build_options": constant(bootstrap, "cbmc_build_options"),
        "tlc": constant(gate, "tlc_version"),
        "tlaps": constant(gate, "tlapm_version"),
        "z3": constant(gate, "z3_version"),
    }


def source_snapshot(repository: Path) -> dict:
    # Git normalizes platform line endings. The full tree binds transitive
    # modules, models/configs, dependency locks, generator and runner sources.
    # Dirty runs are useful diagnostics, but cannot qualify a release.
    facts = formal_sources.source_facts(repository)
    status = facts["status"]
    # Clean Git-byte results retain the old schema2/1 release identity, including
    # LF/CRLF equivalence. Dirty diagnostics now bind index/conflict stages,
    # both tracked deltas and untracked bytes using the one framing owner.
    # Old weak dirty artifacts fail closed; there is no alternate digest basis.
    working = digest_bytes(facts["patch"]) if not status else "sha256:" + formal_sources.framed_digest((
        b"formal-dirty-working-diff-v1", facts["index_entries"], facts["staged"], facts["working"], facts["patch"],
        *formal_sources.untracked_parts(facts["untracked"]),
    ))
    return {
        "commit": facts["revision"].decode().strip(),
        "tree": facts["tree"].decode().strip(),
        "clean": not status,
        "working_diff_digest": working,
        "status_digest": digest_bytes(status),
    }


def committed_file_digest(repository: Path, relative: str, revision: str = "HEAD") -> str:
    # One Git-byte authority for runner, ledger and proof-input identities.
    # Callers qualifying a release must first validate the clean source receipt;
    # this function by itself does not reject dirty or unauthenticated checkouts.
    blob = formal_sources.git_output(repository, "show", revision + ":" + relative)
    return digest_bytes(blob)


def runner_identity(repository: Path) -> dict:
    path = "scripts/ci/check_formal.sh"
    return {"path": path, "digest": committed_file_digest(repository, path)}


def selected_plan_digest(repository: Path, selection: dict) -> str:
    """The digest of the Kani plan a selection derives from ``repository``."""

    try:
        affected = (
            formal_plan.affected_packages(repository, selection["affected_base"])
            if selection["affected_base"] is not None else None
        )
        return formal_plan.digest(formal_plan.batches(repository, selection["tier"], affected))
    except formal_plan.PlanError as error:
        raise ValueError(f"formal evidence plan cannot be derived: {error}") from error


def begin_record(repository: Path, strict: bool, now: int, selection: dict) -> dict:
    if (
        set(selection) != set(FULL_SELECTION)
        or selection["tier"] not in {"full", "pr"}
        or (selection["affected_base"] is not None and selection["tier"] != "pr")
    ):
        raise ValueError("formal evidence selection is a tier, and an affected base only for a pr run")
    return {
        "schema_version": RUN_SCHEMA_VERSION, "run_id": str(uuid.uuid4()), "status": "running",
        "strict": strict, "started_at_unix": now, "selection": dict(selection),
        "source": source_snapshot(repository),
        "proof_toolchain": proof_toolchain(repository),
        "runner": runner_identity(repository),
    }


def finish_record(
    record: dict,
    repository: Path,
    missing_tools: bool,
    now: int,
    exit_code: int = 0,
    failure_kind: str | None = None,
    failure_stage: str | None = None,
    failure_subject: str | None = None,
    deadline_seconds: int | None = None,
) -> dict:
    if record.get("status") != "running" or record.get("schema_version") != RUN_SCHEMA_VERSION:
        raise ValueError("formal evidence must finish one running invocation")
    if type(now) is not int or now < record["started_at_unix"]:
        raise ValueError("formal evidence completion precedes its start")
    if type(exit_code) is not int or exit_code < 0:
        raise ValueError("formal evidence has an invalid exit code")
    if exit_code != 0:
        if failure_kind not in FAILURE_KINDS:
            raise ValueError("failed formal evidence has no valid failure kind")
        if not isinstance(failure_stage, str) or not failure_stage:
            raise ValueError("failed formal evidence has no failure stage")
        outcome = {
            "kind": failure_kind,
            "exit_code": exit_code,
            "stage": failure_stage,
        }
        if failure_subject is not None:
            if not isinstance(failure_subject, str) or not failure_subject:
                raise ValueError("failed formal evidence has an invalid failure subject")
            outcome["subject"] = failure_subject
        if failure_kind == "memory-bound" and (failure_subject is None or exit_code == 124):
            raise ValueError("memory-bound evidence requires an exact exhausted subject and a non-deadline exit")
        if failure_kind == "resource-bound":
            if exit_code != 124 or failure_subject is None:
                raise ValueError("resource-bound evidence requires an exact timed-out subject")
            if type(deadline_seconds) is not int or deadline_seconds <= 0:
                raise ValueError("resource-bound evidence requires a positive deadline")
            outcome["deadline_seconds"] = deadline_seconds
        elif deadline_seconds is not None:
            raise ValueError("only resource-bound evidence may record a deadline")
        return dict(
            record,
            status="not-passed",
            finished_at_unix=now,
            source_unchanged=record["source"] == source_snapshot(repository),
            outcome=outcome,
        )
    if any(value is not None for value in (
        failure_kind, failure_stage, failure_subject, deadline_seconds,
    )):
        raise ValueError("successful formal evidence cannot contain a failure outcome")
    if record.get("strict") is not True or missing_tools:
        raise ValueError("only a complete strict formal run can produce success evidence")
    if record["source"] != source_snapshot(repository):
        raise ValueError("formal evidence source changed during execution")
    if record["proof_toolchain"] != proof_toolchain(repository):
        raise ValueError("formal evidence toolchain contract changed during execution")
    if record["runner"] != runner_identity(repository):
        raise ValueError("formal evidence runner changed during execution")
    # The source is unchanged, so the plan derived now is the plan that ran.
    plan_digest = selected_plan_digest(repository, record["selection"])
    return dict(record, status="passed", finished_at_unix=now, plan_digest=plan_digest)


def validate_record(record: dict, repository: Path, now: int | None = None) -> None:
    if record.get("schema_version") != RUN_SCHEMA_VERSION or record.get("status") != "passed" or record.get("strict") is not True:
        raise ValueError("formal evidence is not a completed strict run")
    if record.get("selection") != FULL_SELECTION:
        raise ValueError("formal evidence is not a full-tier run: a pr or narrowed run cannot qualify a release")
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
    if record.get("plan_digest") != selected_plan_digest(repository, FULL_SELECTION):
        raise ValueError("formal evidence ran another Kani plan than the release source derives")


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


def begin_artifacts(repository: Path, parent: Path, kind: str, directory: Path | None = None,
                    *, observation_only: bool = False, source_identity: dict | None = None) -> Path:
    """One immutable diagnostic bundle per invocation; never a release proof."""
    if source_identity is not None and not observation_only:
        raise ValueError("only diagnostic observations may use a caller-owned source identity")
    parent.mkdir(parents=True, exist_ok=True)
    if directory is None:
        directory = Path(tempfile.mkdtemp(prefix=kind + "-", dir=parent))
    else:
        directory.mkdir(parents=True, exist_ok=False)
    directory = directory.resolve()
    if source_identity is not None:
        source = dict(source_identity)
    else:
        try:
            source = source_snapshot(repository)
        except (OSError, ValueError, KeyError):
            if not observation_only:
                raise
            source = {"available": False, "reason": "source identity unavailable"}
    record = {
        "schema_version": 1, "kind": kind, "status": "running", "run_id": str(uuid.uuid4()),
        "source": source, "platform": {"system": platform.system(), "machine": platform.machine()},
        "started_at_unix": int(time.time()),
    }
    if observation_only:
        record.update(observation_only=True, full_gate=False)
    else:
        record["proof_toolchain"] = proof_toolchain(repository)
    write_record(directory / "evidence.json", record)
    return directory


def artifact_files(directory: Path) -> dict[str, str]:
    files = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink(): raise ValueError("artifact bundle cannot contain symlinks")
        if path.is_file() and path != directory / "evidence.json":
            files[path.relative_to(directory).as_posix()] = digest_file(path)
    return files


def finish_artifacts(directory: Path, repository: Path, exit_code: int) -> dict:
    record = json.loads((directory / "evidence.json").read_text())
    if record["status"] != "running": raise ValueError("artifact invocation already finished")
    try:
        unchanged = record["source"] == source_snapshot(repository) if record["source"].get("available", True) else None
    except (OSError, ValueError, KeyError):
        if not record.get("observation_only"):
            raise
        unchanged = None
    passed = exit_code == 0 and (record.get("observation_only") or unchanged)
    record.update(status="passed" if passed else "failed", exit_code=exit_code,
        source_unchanged=unchanged, finished_at_unix=int(time.time()), files=artifact_files(directory))
    write_record(directory / "evidence.json", record)
    return record


def verify_artifacts(directory: Path, repository: Path) -> dict:
    record = json.loads((directory / "evidence.json").read_text())
    if record.get("observation_only"):
        raise ValueError("performance observations cannot qualify an artifact")
    if record.get("schema_version") != 1 or record.get("status") not in {"passed", "failed"}:
        raise ValueError("artifact invocation is incomplete")
    code, unchanged = record.get("exit_code"), record.get("source_unchanged")
    if type(code) is not int or type(unchanged) is not bool:
        raise ValueError("artifact result mismatch")
    if (record["status"] == "passed") != (code == 0 and unchanged):
        raise ValueError("artifact result mismatch")
    if record.get("proof_toolchain") != proof_toolchain(repository): raise ValueError("artifact toolchain mismatch")
    if record.get("source") != source_snapshot(repository): raise ValueError("artifact source mismatch")
    if record.get("files") != artifact_files(directory): raise ValueError("artifact content mismatch")
    return record  # Failed bundles remain inspectable, never promoted to Passed.


def main() -> None:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("action", choices=("begin", "finish", "artifacts-begin", "artifacts-finish", "artifacts-verify"))
    parser.add_argument("--record", type=Path)
    parser.add_argument("--directory", type=Path)
    parser.add_argument("--parent", type=Path, default=Path("target/test-timings"))
    parser.add_argument("--kind", choices=("formal", "session-observations"), default="formal")
    parser.add_argument("--exit-code", type=int, default=0)
    parser.add_argument("--failure-kind", choices=FAILURE_KINDS)
    parser.add_argument("--failure-stage")
    parser.add_argument("--failure-subject")
    parser.add_argument("--deadline-seconds", type=int)
    parser.add_argument("--strict", choices=("0", "1"), default="0")
    parser.add_argument("--missing-tools", choices=("0", "1"), default="0")
    parser.add_argument("--tier", choices=("full", "pr"))
    parser.add_argument("--affected", metavar="BASE")
    args = parser.parse_args()
    if args.action == "artifacts-begin":
        print(begin_artifacts(ROOT, args.parent, args.kind, args.directory))
        return
    if args.action.startswith("artifacts-"):
        if args.directory is None: parser.error("--directory is required")
        if args.action == "artifacts-finish":
            record = finish_artifacts(args.directory, ROOT, args.exit_code)
            if not record["source_unchanged"]: raise SystemExit("artifact source changed during execution")
        else:
            record = verify_artifacts(args.directory, ROOT)
            print(f"artifact integrity verified: {record['status']}")
        return
    if args.record is None: parser.error("--record is required")
    now = int(time.time())
    if args.action == "begin":
        if args.tier is None:
            parser.error("begin requires --tier full|pr")
        record = begin_record(ROOT, args.strict == "1", now, {"tier": args.tier, "affected_base": args.affected})
    else:
        record = finish_record(
            json.loads(args.record.read_text()), ROOT, args.missing_tools == "1", now,
            args.exit_code, args.failure_kind, args.failure_stage,
            args.failure_subject, args.deadline_seconds,
        )
    write_record(args.record, record)


if __name__ == "__main__":
    main()
