"""Canonical product service executable and Runtime lifecycle fitness rules.

The service-data ownership checker composes this module.  Keeping these rules
together gives the process lifecycle one fitness owner: role-named binaries and
the authority-free Worker must all terminate in the same Runtime builder rather
than growing launcher-local lifecycle paths.
"""

from __future__ import annotations

from pathlib import Path


WORKER_MANIFEST = "crates/bin/awaken-worker/Cargo.toml"
WORKER_MAIN = "crates/bin/awaken-worker/src/main.rs"
SERVICE_LIFECYCLE_SOURCE = "crates/server/awaken-service-lifecycle/src/lib.rs"
CLI_PROCESS_STARTUP = "crates/bin/awaken-cli/src/process_startup.rs"
CLI_MANIFEST = "crates/bin/awaken-cli/Cargo.toml"
CLI_SERVICE = "crates/bin/awaken-cli/src/service.rs"
CONTROL_BIN = "crates/bin/awaken-cli/src/bin/awaken-control.rs"
COORDINATOR_BIN = "crates/bin/awaken-cli/src/bin/awaken-coordinator.rs"


def service_binary_violations(
    manifest: dict,
    service_source: str,
    control_source: str,
    coordinator_source: str,
) -> list[str]:
    """Keep one lifecycle implementation behind the role-named executables."""

    errors: list[str] = []
    bins = {
        entry.get("name"): entry.get("path")
        for entry in manifest.get("bin", [])
        if isinstance(entry, dict)
    }
    expected = {
        "awaken": "src/main.rs",
        "awaken-control": "src/bin/awaken-control.rs",
        "awaken-coordinator": "src/bin/awaken-coordinator.rs",
    }
    for name, path in expected.items():
        if bins.get(name) != path:
            errors.append(f"missing canonical `{name}` executable at `{path}`")
    if manifest.get("package", {}).get("default-run") != "awaken":
        errors.append("aggregate operator launcher must remain Cargo's default executable")
    if "awaken-worker" in bins:
        errors.append("aggregate CLI package must not recreate the Worker executable")
    if service_source.count("pub async fn run_service(") != 1:
        errors.append("CLI must own exactly one shared service lifecycle")
    if service_source.count("async fn migrate_service_for_role(") != 1:
        errors.append("role executables must share one role-fenced migration lifecycle")
    for name, source in (
        ("awaken-control", control_source),
        ("awaken-coordinator", coordinator_source),
    ):
        if "run_service_binary(" not in source:
            errors.append(f"`{name}` bypasses the shared service lifecycle")
        if "build_" in source:
            errors.append(f"`{name}` reconstructs applications inside its thin entrypoint")
    return errors


def service_runtime_violations(
    worker_manifest: dict,
    worker_main: str,
    lifecycle_source: str,
    cli_process_startup: str,
) -> list[str]:
    """Keep one stack-safe Tokio Runtime owner for every service launcher."""

    errors: list[str] = []
    if "awaken-service-lifecycle" not in worker_manifest.get("dependencies", {}):
        errors.append("awaken-worker must depend on the canonical service Runtime owner")
    if "#[tokio::main" in worker_main:
        errors.append("awaken-worker must not recreate Tokio's default-stack Runtime")
    if worker_main.count("awaken_service_lifecycle::block_on_service(") != 1:
        errors.append("awaken-worker must enter exactly one canonical service Runtime")
    if lifecycle_source.count("pub fn block_on_service") != 1:
        errors.append("service lifecycle must own exactly one public Runtime builder")
    if lifecycle_source.count(".thread_stack_size(") != 1:
        errors.append("service lifecycle must own exactly one explicit Worker stack budget")
    if "pub use awaken_service_lifecycle::block_on_service;" not in cli_process_startup:
        errors.append("aggregate service launchers must reuse the service-lifecycle Runtime")
    return errors


def selftest() -> None:
    """Exercise the cause/effect table beside the lifecycle rules it covers."""

    # Binary-topology cause/effect table: C1 each canonical target has its exact
    # path; C2 `awaken` is the default; C3 there is no Worker twin; C4 service
    # and migration lifecycles each have one owner; C5 both role entrypoints
    # delegate and construct no application. E1 all true => accept. E2 any C1-C5
    # mutation => reject before a parallel product lifecycle can ship. R1 is the
    # accepted row; R2-R15 independently mutate every predicate/count boundary.
    canonical_manifest = {
        "package": {"default-run": "awaken"},
        "bin": [
            {"name": "awaken", "path": "src/main.rs"},
            {"name": "awaken-control", "path": "src/bin/awaken-control.rs"},
            {
                "name": "awaken-coordinator",
                "path": "src/bin/awaken-coordinator.rs",
            },
        ],
    }
    canonical_service = (
        "pub async fn run_service( async fn migrate_service_for_role("
    )
    canonical_entry = "run_service_binary("
    assert service_binary_violations(
        canonical_manifest,
        canonical_service,
        canonical_entry,
        canonical_entry,
    ) == []  # R1/E1
    for rule, target in enumerate(
        ("awaken", "awaken-control", "awaken-coordinator"), start=2
    ):
        missing = {
            "package": canonical_manifest["package"],
            "bin": [
                entry
                for entry in canonical_manifest["bin"]
                if entry["name"] != target
            ],
        }
        assert any(
            f"missing canonical `{target}`" in error
            for error in service_binary_violations(
                missing, canonical_service, canonical_entry, canonical_entry
            )
        ), f"R{rule}/E2 missing target"
    for rule, target in enumerate(
        ("awaken", "awaken-control", "awaken-coordinator"), start=5
    ):
        wrong_path = {
            "package": canonical_manifest["package"],
            "bin": [
                {**entry, "path": "src/bin/wrong.rs"}
                if entry["name"] == target
                else entry
                for entry in canonical_manifest["bin"]
            ],
        }
        assert any(
            f"missing canonical `{target}`" in error
            for error in service_binary_violations(
                wrong_path, canonical_service, canonical_entry, canonical_entry
            )
        ), f"R{rule}/E2 wrong target path"
    binary_mutations = (
        (
            "R8",
            {**canonical_manifest, "package": {"default-run": "awaken-control"}},
            canonical_service,
            canonical_entry,
            canonical_entry,
            "aggregate operator launcher",
        ),
        (
            "R9",
            {
                **canonical_manifest,
                "bin": canonical_manifest["bin"]
                + [{"name": "awaken-worker", "path": "worker.rs"}],
            },
            canonical_service,
            canonical_entry,
            canonical_entry,
            "must not recreate the Worker executable",
        ),
        (
            "R10",
            canonical_manifest,
            "async fn migrate_service_for_role(",
            canonical_entry,
            canonical_entry,
            "exactly one shared service lifecycle",
        ),
        (
            "R11",
            canonical_manifest,
            canonical_service + " pub async fn run_service(",
            canonical_entry,
            canonical_entry,
            "exactly one shared service lifecycle",
        ),
        (
            "R12",
            canonical_manifest,
            "pub async fn run_service(",
            canonical_entry,
            canonical_entry,
            "one role-fenced migration lifecycle",
        ),
        (
            "R13",
            canonical_manifest,
            canonical_service + " async fn migrate_service_for_role(",
            canonical_entry,
            canonical_entry,
            "one role-fenced migration lifecycle",
        ),
        (
            "R14-control-bypass",
            canonical_manifest,
            canonical_service,
            "",
            canonical_entry,
            "`awaken-control` bypasses",
        ),
        (
            "R14-coordinator-bypass",
            canonical_manifest,
            canonical_service,
            canonical_entry,
            "",
            "`awaken-coordinator` bypasses",
        ),
        (
            "R15-control-builder",
            canonical_manifest,
            canonical_service,
            canonical_entry + " build_control(",
            canonical_entry,
            "`awaken-control` reconstructs",
        ),
        (
            "R15-coordinator-builder",
            canonical_manifest,
            canonical_service,
            canonical_entry,
            canonical_entry + " build_coordinator(",
            "`awaken-coordinator` reconstructs",
        ),
    )
    for rule, manifest, service, control, coordinator, expected in binary_mutations:
        assert any(
            expected in error
            for error in service_binary_violations(
                manifest, service, control, coordinator
            )
        ), f"{rule}/E2"

    # Runtime cause/effect table: C6 Worker names the shared dependency; C7 it
    # has no local Tokio macro and enters the owner exactly once; C8 the owner
    # has exactly one public builder and stack budget; C9 aggregate launchers
    # re-export it. E3 all true => accept; E4 any false/count != 1 => reject.
    # R16 is accepted; R17-R25 cover every independent failure predicate and
    # both missing/duplicate sides of each exact-count condition.
    worker_manifest = {
        "dependencies": {"awaken-service-lifecycle": {"workspace": True}}
    }
    worker_main = "awaken_service_lifecycle::block_on_service(async {})"
    lifecycle = "pub fn block_on_service .thread_stack_size("
    cli_reexport = "pub use awaken_service_lifecycle::block_on_service;"
    assert service_runtime_violations(
        worker_manifest, worker_main, lifecycle, cli_reexport
    ) == []  # R16/E3
    runtime_mutations = (
        ("R17", {}, worker_main, lifecycle, cli_reexport, "must depend"),
        (
            "R18",
            worker_manifest,
            "#[tokio::main] " + worker_main,
            lifecycle,
            cli_reexport,
            "must not recreate",
        ),
        ("R19", worker_manifest, "", lifecycle, cli_reexport, "enter exactly one"),
        (
            "R20",
            worker_manifest,
            worker_main + worker_main,
            lifecycle,
            cli_reexport,
            "enter exactly one",
        ),
        ("R21", worker_manifest, worker_main, "", cli_reexport, "one public Runtime"),
        (
            "R22",
            worker_manifest,
            worker_main,
            "pub fn block_on_service pub fn block_on_service .thread_stack_size(",
            cli_reexport,
            "one public Runtime",
        ),
        (
            "R23",
            worker_manifest,
            worker_main,
            "pub fn block_on_service",
            cli_reexport,
            "one explicit Worker stack",
        ),
        (
            "R24",
            worker_manifest,
            worker_main,
            lifecycle + " .thread_stack_size(",
            cli_reexport,
            "one explicit Worker stack",
        ),
        (
            "R25",
            worker_manifest,
            worker_main,
            lifecycle,
            "fn block_on_service() {}",
            "must reuse the service-lifecycle Runtime",
        ),
    )
    for rule, manifest, main, owner, startup, expected in runtime_mutations:
        assert any(
            expected in error
            for error in service_runtime_violations(manifest, main, owner, startup)
        ), f"{rule}/E4"


def check_all(
    repo_root: Path,
    cli_manifest: dict,
    worker_manifest: dict,
) -> list[str]:
    """Check the complete service-launcher family through its canonical files."""

    errors: list[str] = []
    for error in service_binary_violations(
        cli_manifest,
        (repo_root / CLI_SERVICE).read_text(encoding="utf-8"),
        (repo_root / CONTROL_BIN).read_text(encoding="utf-8"),
        (repo_root / COORDINATOR_BIN).read_text(encoding="utf-8"),
    ):
        errors.append(f"{CLI_MANIFEST}: {error}")
    for error in service_runtime_violations(
        worker_manifest,
        (repo_root / WORKER_MAIN).read_text(encoding="utf-8"),
        (repo_root / SERVICE_LIFECYCLE_SOURCE).read_text(encoding="utf-8"),
        (repo_root / CLI_PROCESS_STARTUP).read_text(encoding="utf-8"),
    ):
        errors.append(f"Service Runtime: {error}")
    return errors
