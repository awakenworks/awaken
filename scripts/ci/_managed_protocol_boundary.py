"""Semantic route and application boundaries for the Managed protocol adapter."""

import json
import re
from pathlib import Path

import _execution_ownership_fitness


AWAKEN_MANAGED_EXTENSION_ROUTES = frozenset(
    {
        ("GET", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("PUT", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("GET", "/v1/awaken/sessions/{}/live-inbox"),
        ("POST", "/v1/awaken/sessions"),
        ("POST", "/v1/awaken/sessions/{}/live-inbox"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/order"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("DELETE", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("PUT", "/v1/awaken/sessions/{}/resources"),
        ("POST", "/v1/awaken/sandbox-execution-policies"),
        ("POST", "/v1/awaken/sandbox-execution-policies/{}/versions"),
        ("GET", "/v1/awaken/sandbox-execution-policies/{}/versions/{}"),
        ("GET", "/v1/awaken/environments/{}/sandbox-execution-policy"),
        ("POST", "/v1/awaken/environments/{}/sandbox-execution-policy"),
    }
)

MANAGED_STATE_ROOT = "crates/server/awaken-protocol-managed/src/state"
FORBIDDEN_APPLICATION_BYPASSES = {
    "runtime": "use a SessionApplication command/query instead of the Runtime collaborator",
    "session_repository": "use the SessionApplication aggregate command/query surface",
    "config_source": "use a SessionApplication publication query",
    "credential_source": "use a SessionApplication credential command/query",
    "resource_catalog": "use a SessionApplication resource command/query",
    "repository_credential_ingress": "use the SessionApplication repository command",
    "lifecycle_sink": "use the SessionApplication lifecycle projection command",
}


def managed_application_bypass_violations(sources: dict[str, str]) -> list[str]:
    """Managed wire state may call the application, never its raw collaborators."""
    errors: list[str] = []
    for relative, source in sources.items():
        production = _execution_ownership_fitness._production(source)
        for method, replacement in FORBIDDEN_APPLICATION_BYPASSES.items():
            pattern = re.compile(
                rf"\.application\s*\.\s*{re.escape(method)}\s*\(", re.MULTILINE
            )
            if pattern.search(production):
                errors.append(
                    f"{relative}: Managed state bypasses SessionApplication via {method}(); {replacement}"
                )
    return errors


def check_managed_application_boundary(repo_root: Path) -> list[str]:
    root = repo_root / MANAGED_STATE_ROOT
    sources: dict[str, str] = {}
    for path in sorted(root.glob("**/*.rs")):
        if "tests" in path.parts:
            continue
        sources[str(path.relative_to(repo_root))] = path.read_text(encoding="utf-8")
    return managed_application_bypass_violations(sources)


def session_admission_ownership_violations(sources: dict[str, str]) -> list[str]:
    """SessionApplication, never a protocol adapter, owns Run admission."""
    errors: list[str] = []
    for relative, source in sources.items():
        production = _execution_ownership_fitness._production(source)
        for symbol in ("prepare_protocol_session", "ManagedSessionAdmission"):
            if re.search(rf"\b{symbol}\b", production):
                errors.append(
                    f"{relative}: obsolete protocol-owned Session admission {symbol!r}; "
                    "use SessionApplication's SessionRunAdmission implementation"
                )
    application = sources.get(
        "crates/server/awaken-session-application/src/run_admission.rs", ""
    )
    if "impl SessionRunAdmission for SessionApplication" not in application:
        errors.append(
            "awaken-session-application: missing authoritative SessionRunAdmission implementation"
        )
    coordinator = sources.get("crates/server/awaken-coordinator/src/lib.rs", "")
    if "managed_state.session_application()" not in coordinator:
        errors.append(
            "awaken-coordinator: public Run protocols are not wired to SessionApplication admission"
        )
    return errors


def check_session_admission_ownership(repo_root: Path) -> list[str]:
    paths = (
        repo_root / "crates/server/awaken-protocol-managed/src/state/sessions.rs",
        repo_root / "crates/server/awaken-coordinator/src/lib.rs",
        repo_root / "crates/server/awaken-session-application/src/run_admission.rs",
    )
    return session_admission_ownership_violations(
        {
            str(path.relative_to(repo_root)): path.read_text(encoding="utf-8")
            for path in paths
        }
    )


def managed_route_inventory_violations(
    expected_core: set[tuple[str, str]],
    core: set[tuple[str, str]],
    extensions: set[tuple[str, str]],
) -> list[str]:
    errors: list[str] = []
    for route in sorted(core - expected_core):
        errors.append(f"non-Anthropic route in managed core: {route[0]} {route[1]}")
    for route in sorted(expected_core - core):
        errors.append(f"missing Anthropic managed route owner: {route[0]} {route[1]}")
    for route in sorted(extensions - AWAKEN_MANAGED_EXTENSION_ROUTES):
        errors.append(f"unlisted Awaken managed extension route: {route[0]} {route[1]}")
    for route in sorted(AWAKEN_MANAGED_EXTENSION_ROUTES - extensions):
        errors.append(f"missing listed Awaken managed extension route: {route[0]} {route[1]}")
    for method, path in sorted(extensions):
        if not path.startswith("/v1/awaken/"):
            errors.append(f"Awaken extension is not explicitly namespaced: {method} {path}")
    return errors


def official_managed_routes(repo_root: Path) -> set[tuple[str, str]]:
    manifest = json.loads(
        (
            repo_root
            / "contracts/anthropic-managed/upstream-oracle.generated.json"
        ).read_text(encoding="utf-8")
    )
    operations = (
        manifest["current"]["operations"]
        + [
            operation
            for anchor in manifest["anchors"]
            for operation in anchor["only_in_anchor"]
        ]
        + manifest["documented_routes"]
    )
    return {
        (operation["method"], operation["path"].replace("{*}", "{}"))
        for operation in operations
    }


def check_managed_route_inventory(repo_root: Path) -> list[str]:
    compatible_roots = (repo_root / "crates/server/awaken-protocol-managed/src",)
    extension_root = repo_root / "crates/server/awaken-protocol-awaken/src"
    core: set[tuple[str, str]] = set()
    for root in compatible_roots:
        for path in sorted(root.glob("**/*.rs")):
            core.update(_execution_ownership_fitness._owned_routes(path))
    extensions: set[tuple[str, str]] = set()
    for path in sorted(extension_root.glob("**/*.rs")):
        extensions.update(_execution_ownership_fitness._owned_routes(path))
    normalized_core = {
        (method, route.replace("{*}", "{}")) for method, route in core
    }
    return managed_route_inventory_violations(
        official_managed_routes(repo_root), normalized_core, extensions
    )


def selftest() -> None:
    # Cause/effect graph and decision table:
    # C1 exact official compatible protocol-family inventory, C2 exact separately packaged and
    # namespaced extension inventory, C3 unknown core route, C4 unknown extension.
    # R1 C1+C2 -> accept; R2 C3 -> reject compatibility contamination;
    # R3 C4 -> reject implicit extension; R4 missing declared route -> reject drift.
    official = {("GET", "/v1/sessions"), ("POST", "/v1/sessions")}
    assert managed_route_inventory_violations(
        official, set(official), set(AWAKEN_MANAGED_EXTENSION_ROUTES)
    ) == [], "R1"
    assert managed_route_inventory_violations(
        official,
        set(official) | {("GET", "/v1/custom")},
        set(AWAKEN_MANAGED_EXTENSION_ROUTES),
    ), "R2"
    assert managed_route_inventory_violations(
        official,
        set(official),
        set(AWAKEN_MANAGED_EXTENSION_ROUTES) | {("GET", "/v1/custom")},
    ), "R3"
    assert managed_route_inventory_violations(official, set(), set()), "R4"

    # Application-boundary cause/effect graph: C5 production wire state calls a
    # semantic SessionApplication operation; C6 it reaches through the application
    # to a raw Runtime/repository/source/sink; C7 the same text occurs only in an
    # inline test module. Effects: E5 accept the single application authority;
    # E6 reject the parallel orchestration path; E7 ignore fixture inspection.
    # Decision table: B1 C5,!C6 -> E5; B2 C6 -> E6; B3 C7 -> E7.
    assert managed_application_bypass_violations(
        {"state.rs": "self.application.session(id).await"}
    ) == [], "B1/E5"
    assert managed_application_bypass_violations(
        {"state.rs": "self.application.session_repository().get(id).await"}
    ), "B2/E6"
    assert managed_application_bypass_violations(
        {
            "state.rs": "prod\n#[cfg(test)]\nmod tests { self.application.runtime(); }"
        }
    ) == [], "B3/E7"

    # Admission-ownership cause/effect graph: C8 SessionApplication implements
    # the neutral gate; C9 Coordinator injects that implementation; C10 Managed
    # or Coordinator defines a compatibility admission. Effects: E8 one owner;
    # E9 all public Run adapters share it; E10 reject a second creation/recovery
    # path. Decision table: A1 C8+C9,!C10 -> accept; A2 !C8 -> reject; A3 C10 ->
    # reject. The production stripper keeps test fixtures from becoming owners.
    valid = {
        "crates/server/awaken-session-application/src/run_admission.rs":
            "impl SessionRunAdmission for SessionApplication {}",
        "crates/server/awaken-coordinator/src/lib.rs":
            "managed_state.session_application()",
        "crates/server/awaken-protocol-managed/src/state/sessions.rs": "",
    }
    assert session_admission_ownership_violations(valid) == [], "A1/E8/E9"
    missing = dict(valid)
    missing["crates/server/awaken-session-application/src/run_admission.rs"] = ""
    assert session_admission_ownership_violations(missing), "A2/E10"
    duplicate = dict(valid)
    duplicate["crates/server/awaken-protocol-managed/src/state/sessions.rs"] = (
        "fn prepare_protocol_session() {}"
    )
    assert session_admission_ownership_violations(duplicate), "A3/E10"
