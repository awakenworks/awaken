"""Semantic route and application boundaries for the Managed protocol adapter."""

import json
import re
import tempfile
from pathlib import Path

import _crate_boundary_workspace
import _execution_ownership_fitness


AWAKEN_MANAGED_EXTENSION_ROUTES = frozenset(
    {
        ("GET", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("GET", "/v1/awaken/repositories"),
        ("POST", "/v1/awaken/repositories"),
        ("GET", "/v1/awaken/repositories/{}"),
        ("GET", "/v1/awaken/memory-stores/{}/copy-conflicts"),
        ("PUT", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("GET", "/v1/awaken/sessions/{}/live-inbox"),
        ("GET", "/v1/awaken/sessions/{}/execution-evidence"),
        ("GET", "/v1/awaken/sessions/{}/content-capture"),
        ("POST", "/v1/awaken/sessions"),
        ("POST", "/v1/awaken/sessions/{}/release"),
        ("POST", "/v1/awaken/sessions/{}/runs"),
        ("GET", "/v1/awaken/sessions/{}/runs/{}/events"),
        ("POST", "/v1/awaken/sessions/{}/runs/{}/resume"),
        ("POST", "/v1/awaken/sessions/{}/event-admissions/close"),
        ("POST", "/v1/awaken/sessions/{}/live-inbox"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/order"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("DELETE", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("PUT", "/v1/awaken/sessions/{}/resources"),
        ("GET", "/v1/awaken/sessions/{}/resources"),
        ("POST", "/v1/awaken/sandbox-execution-policies"),
        ("POST", "/v1/awaken/sandbox-execution-policies/{}/versions"),
        ("GET", "/v1/awaken/sandbox-execution-policies/{}/versions/{}"),
        ("GET", "/v1/awaken/environments/{}/sandbox-execution-policy"),
        ("POST", "/v1/awaken/environments/{}/sandbox-execution-policy"),
        ("POST", "/v1/awaken/environments"),
    }
)

# Managed state reaching past the Session application to its raw collaborators
# (Runtime, repository, credential or Resource sources) is not checked here:
# the adapter holds only the application's role traits
# (crates/server/awaken-session-application/src/roles.rs), which expose none of
# them, and the collaborators are crate-private to the application, so such a
# call does not compile.


# The co-located root that selects the Coordinator's execution ports, and the
# ports themselves.
COORDINATOR_EXECUTION_ROOT = "crates/bin/awaken-process-startup/src/coordinator_execution.rs"
COORDINATOR_EXECUTION_PORTS = "crates/server/awaken-coordinator/src/execution_ports.rs"


def session_admission_ownership_violations(sources: dict[str, str]) -> list[str]:
    """SessionApplication, never a protocol adapter, owns Run admission."""
    errors: list[str] = []
    for relative, source in sources.items():
        production = _crate_boundary_workspace.production_rust(source)
        for symbol in ("prepare_protocol_session", "ManagedSessionAdmission"):
            if re.search(rf"\b{symbol}\b", production):
                errors.append(
                    f"{relative}: obsolete protocol-owned Session admission {symbol!r}; "
                    "use SessionApplication's AdmitSessionRun command"
                )
    application = sources.get(
        "crates/server/awaken-session-application/src/run_admission.rs", ""
    )
    required = (
        "pub async fn admit_session_run_for_owner",
        "pub struct SessionRunApplication",
        "impl RunApplication for SessionRunApplication",
    )
    if any(symbol not in application for symbol in required):
        errors.append(
            "awaken-session-application: missing the authoritative Session Run command or adapter"
        )
    # The composition root that builds the executor builds the one public Run
    # adapter over the Session application it injected, and the Coordinator
    # receives that adapter typed as SessionRunApplication, so every public Run
    # protocol it mounts admits through the application. The Managed adapter
    # holds only narrowed roles and is never the source of that application.
    root = sources.get(COORDINATOR_EXECUTION_ROOT, "")
    ports = sources.get(COORDINATOR_EXECUTION_PORTS, "")
    if "SessionRunApplication::new(" not in _crate_boundary_workspace.production_rust(root) or (
        not re.search(r"pub session_runs:\s*Arc<SessionRunApplication>", ports)
    ):
        errors.append(
            "awaken-coordinator: public Run protocols are not wired to SessionApplication admission"
        )
    coordinator = sources.get("crates/server/awaken-coordinator/src/lib.rs", "")
    if "managed_state.session_application()" in _crate_boundary_workspace.production_rust(
        coordinator
    ):
        errors.append(
            "awaken-coordinator: the Session application is read back out of the Managed "
            "adapter; take it from the composition root"
        )
    return errors


def check_session_admission_ownership(repo_root: Path) -> list[str]:
    paths = (
        repo_root / "crates/server/awaken-protocol-managed/src/state/sessions.rs",
        repo_root / "crates/server/awaken-coordinator/src/lib.rs",
        repo_root / "crates/server/awaken-session-application/src/run_admission.rs",
        repo_root / COORDINATOR_EXECUTION_ROOT,
        repo_root / COORDINATOR_EXECUTION_PORTS,
    )
    return session_admission_ownership_violations(
        {
            _crate_boundary_workspace.repository_path(path, repo_root): path.read_text(encoding="utf-8")
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
    compatible_root = repo_root / "crates/server/awaken-protocol-managed/src"
    # Awaken's /v1/awaken routes: the Managed-protocol extensions (in the
    # Managed adapter, speaking its envelope) and Awaken's own routes.
    extension_roots = (
        compatible_root / "awaken_extensions",
        repo_root / "crates/server/awaken-protocol-awaken/src",
    )
    core: set[tuple[str, str]] = set()
    for path in sorted(compatible_root.glob("**/*.rs")):
        if extension_roots[0] not in path.parents:
            core.update(_execution_ownership_fitness._owned_routes(path))
    extensions: set[tuple[str, str]] = set()
    for root in extension_roots:
        for path in sorted(root.glob("**/*.rs")):
            extensions.update(_execution_ownership_fitness._owned_routes(path))
    normalized_core = {
        (method, route.replace("{*}", "{}")) for method, route in core
    }
    return managed_route_inventory_violations(
        official_managed_routes(repo_root), normalized_core, extensions
    )


def _rust_struct_fields(source: str, name: str) -> set[str]:
    declaration = re.search(rf"\bstruct\s+{name}\s*\{{([^}}]*)\}}", source, re.S)
    if declaration is None:
        return set()
    return set(
        re.findall(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?([A-Za-z_]\w*)\s*:",
            declaration.group(1),
        )
    )


def _rust_function_body_spans(source: str, names: set[str]) -> list[tuple[int, int]]:
    """Find the owned function bodies, ignoring braces inside ordinary strings."""
    spans: list[tuple[int, int]] = []
    for function in re.finditer(r"\bfn\s+([A-Za-z_]\w*)\b", source):
        if function.group(1) not in names:
            continue
        opening = source.find("{", function.end())
        if opening < 0:
            continue
        depth = 0
        quoted = False
        escaped = False
        for position in range(opening, len(source)):
            char = source[position]
            if quoted:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    spans.append((opening, position + 1))
                    break
    return spans


def projection_input_boundary_violations(
    entrypoint: str,
    record: str,
    reducers: dict[str, str],
    adjacent_modules: dict[str, str] | None = None,
) -> list[str]:
    """Keep published SessionRecord content out of committed fact selection."""
    errors: list[str] = []
    test_only_modules = set(
        re.findall(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+(\w+)\s*;", entrypoint)
    )
    entrypoint = _crate_boundary_workspace.production_rust(entrypoint)
    record = _crate_boundary_workspace.production_rust(record)
    memo_fields = _rust_struct_fields(record, "ProjectionDeliveryMemo")
    allowed_memo = {
        "cache_revision", "transient_events", "transient_owners"
    }
    if memo_fields != allowed_memo:
        errors.append(
            "Managed projection warm memo must carry only cache CAS and transient preview fields"
        )
    if _rust_struct_fields(entrypoint, "CommittedPrefixBase") != {"owner_scope", "record", "memo"}:
        errors.append("Managed committed-prefix base must own root scope, fresh record and delivery memo")
    if not re.search(r"\bfn\s+into_delivery_memo\s*\(\s*self\s*\)", record):
        errors.append("SessionRecord must consume warm state through into_delivery_memo")
    if not re.search(r"let\s+Self\s*\{[^}]*\}\s*=\s*self", record, re.S):
        errors.append("warm-to-delivery-memo extraction must destructure every SessionRecord field")
    if not re.search(
        r"\bfn\s+build_committed_projection_candidate_over\s*\([^)]*base\s*:\s*CommittedPrefixBase",
        entrypoint,
        re.S,
    ):
        errors.append("the sole candidate reducer must accept CommittedPrefixBase, not a warm SessionRecord")
    for required in (
        "read_persisted_session_projection(session_id)",
        "rehydrated_session_for(",
        "SessionRecord::new(",
        ".into_delivery_memo()",
    ):
        if required not in entrypoint:
            errors.append(f"Managed projection lost its root-derived clean base: {required}")
    if re.search(r"\b(?:base_record|warm_record|published_record)\s*\.\s*(?:events|child_threads|checkpoint|permission_decisions)", entrypoint):
        errors.append("Managed candidate reads durable projection fields from its warm record")
    if re.search(r"\bself\.owner_scope\s*\(", entrypoint):
        errors.append("Managed candidate reads owner scope from its warm map, not the durable root")
    domain_call = "awaken_managed_session_events::derive_coordination_events("
    direct_count = entrypoint.count(domain_call)
    delegated_count = len(re.findall(r"\bevidence\s*\.\s*derive_plans\s*\(", entrypoint))
    evidence_sources = [
        _crate_boundary_workspace.production_rust(source)
        for relative, source in {**reducers, **(adjacent_modules or {})}.items()
        if relative.endswith("candidate_evidence.rs")
    ]
    bodies = [source[start:end] for source in evidence_sources
              for start, end in _rust_function_body_spans(source, {"derive_plans"})]
    direct_plan = direct_count == 1 and delegated_count == 0
    delegated_plan = (direct_count == 0 and delegated_count == 1 and len(bodies) == 1
                      and bodies[0].count(domain_call) == 1
                      and sum(source.count(domain_call) for source in evidence_sources) == 1)
    if not (direct_plan or delegated_plan):
        errors.append("Managed candidate lost its single committed-prefix domain plan")
    for relative, source in (adjacent_modules or {}).items():
        production = _crate_boundary_workspace.production_rust(source)
        if re.search(r"\b(?:issued_tool_events|coordination_orders|cross_posts)\s*\(", production):
            errors.append(
                f"{relative}: coordination fact selection must consume the committed-prefix plan"
            )
        # Child status lives in a sibling of committed_projection.rs. Discover
        # that sibling from the owning events directory below, rather than
        # keeping an allow/deny list of files that would miss a module split.
        for start, end in _rust_function_body_spans(
            production,
            {
                "plan_child_activity_projection",
                "append_child_disposition_projection",
                "append_parent_terminal_projection",
            },
        ):
            if re.search(r"\brecord\s*\.\s*child_threads\b", production[start:end]):
                errors.append(
                    f"{relative}: child fact selection reads rendered child Thread status"
                )
    for relative, source in reducers.items():
        if Path(relative).stem in test_only_modules:
            continue
        production = _crate_boundary_workspace.production_rust(source)
        # CAS publication and prior-complete observation are delivery owners;
        # permit their bodies only. A second reducer in the same file cannot
        # inherit an exception just by sharing that file with the publisher.
        delivery_bodies = _rust_function_body_spans(
            production,
            {
                "publish_projection_candidate",
                "publish_projection_update",
                "refresh_committed_events_for_observation",
            },
        )
        for warm_read in re.finditer(r"\.\s*sessions\b", production):
            if not any(start <= warm_read.start() < end for start, end in delivery_bodies):
                errors.append(f"{relative}: committed reducer reads the warm Session cache")
        if re.search(r"\b(?:base_record|warm_record|published_record)\s*\.\s*(?:events|child_threads|checkpoint|permission_decisions)", production):
            errors.append(f"{relative}: committed reducer reads warm durable projection fields")
    return errors


def check_projection_input_boundary(repo_root: Path) -> list[str]:
    # The module declaration is the scope owner. Its same-stem directory is
    # walked in full, so a newly split reducer file enters the guard at once.
    module = (
        repo_root
        / "crates/server/awaken-protocol-managed/src/state/events/committed_projection.rs"
    )
    record = module.parent.parent / "session_record.rs"
    reducers = {
        _crate_boundary_workspace.repository_path(path, repo_root): path.read_text(encoding="utf-8")
        for path in sorted(module.with_suffix("").rglob("*.rs"))
    }
    adjacent_modules = {
        _crate_boundary_workspace.repository_path(path, repo_root): path.read_text(encoding="utf-8")
        for path in sorted(module.parent.rglob("*.rs"))
    }
    return projection_input_boundary_violations(
        module.read_text(encoding="utf-8"),
        record.read_text(encoding="utf-8"),
        reducers,
        adjacent_modules,
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

    # R5 Repository list/create/retrieve each belong to the explicit Awaken
    # extension inventory. Moving any one into compatibility core must report
    # contamination; omitting it must report a missing extension. The exact
    # method/path table is independent of the production extension catalog.
    repository_routes = {
        ("GET", "/v1/awaken/repositories"),
        ("POST", "/v1/awaken/repositories"),
        ("GET", "/v1/awaken/repositories/{}"),
    }
    assert repository_routes <= AWAKEN_MANAGED_EXTENSION_ROUTES, "R5 registered"
    for route in repository_routes:
        remaining = set(AWAKEN_MANAGED_EXTENSION_ROUTES) - {route}
        assert f"non-Anthropic route in managed core: {route[0]} {route[1]}" in (
            managed_route_inventory_violations(official, set(official) | {route}, remaining)
        ), "R5 wrong owner"
        assert f"missing listed Awaken managed extension route: {route[0]} {route[1]}" in (
            managed_route_inventory_violations(official, set(official), remaining)
        ), "R5 omitted"

    # R6 the read-only Session execution projection is an explicit Awaken
    # extension. Removing it from either production or this sole inventory must
    # fail rather than silently making a product evidence claim unverifiable.
    execution_evidence_route = (
        "GET", "/v1/awaken/sessions/{}/execution-evidence"
    )
    assert execution_evidence_route in AWAKEN_MANAGED_EXTENSION_ROUTES, "R6 registered"
    without_execution_evidence = set(AWAKEN_MANAGED_EXTENSION_ROUTES) - {
        execution_evidence_route
    }
    assert (
        f"missing listed Awaken managed extension route: "
        f"{execution_evidence_route[0]} {execution_evidence_route[1]}"
        in managed_route_inventory_violations(
            official, set(official), without_execution_evidence
        )
    ), "R6 omitted"

    # Admission-ownership cause/effect graph: C8 SessionApplication owns the
    # neutral command and sole adapter; C9 the composition root builds that
    # adapter and the Coordinator receives it typed (A4 unbuilt, A6 erased);
    # C10 Managed or Coordinator defines a compatibility admission. Effects: E8 one owner;
    # E9 all public Run adapters share it; E10 reject a second creation/recovery
    # path. Decision table: A1 C8+C9,!C10 -> accept; A2 !C8 -> reject; A3 C10 ->
    # reject. The production stripper keeps test fixtures from becoming owners.
    valid = {
        "crates/server/awaken-session-application/src/run_admission.rs":
            "pub async fn admit_session_run_for_owner() {} "
            "pub struct SessionRunApplication; "
            "impl RunApplication for SessionRunApplication {}",
        "crates/server/awaken-coordinator/src/lib.rs": "let session_runs = execution.session_runs;",
        COORDINATOR_EXECUTION_ROOT: "let runs = Arc::new(SessionRunApplication::new(runs, sessions));",
        COORDINATOR_EXECUTION_PORTS: "pub session_runs: Arc<SessionRunApplication>,",
        "crates/server/awaken-protocol-managed/src/state/sessions.rs": "",
    }
    assert session_admission_ownership_violations(valid) == [], "A1/E8/E9"
    missing = dict(valid)
    missing["crates/server/awaken-session-application/src/run_admission.rs"] = ""
    assert session_admission_ownership_violations(missing), "A2/E10"
    unwired = dict(valid)
    unwired[COORDINATOR_EXECUTION_ROOT] = ""
    assert session_admission_ownership_violations(unwired), "A4/!E9"
    erased = dict(valid)
    erased[COORDINATOR_EXECUTION_PORTS] = "pub session_runs: Arc<dyn RunApplication>,"
    assert session_admission_ownership_violations(erased), "A6/!E9 erased port type"
    read_back = dict(valid)
    read_back["crates/server/awaken-coordinator/src/lib.rs"] += (
        "; let sessions = managed_state.session_application();"
    )
    assert session_admission_ownership_violations(read_back), "A5/!E9"
    duplicate = dict(valid)
    duplicate["crates/server/awaken-protocol-managed/src/state/sessions.rs"] = (
        "fn prepare_protocol_session() {}"
    )
    assert session_admission_ownership_violations(duplicate), "A3/E10"

    # Projection-input cause/effect table. C1 the reducer takes only a fresh
    # committed-prefix base and the warm memo has delivery fields; C2 a new
    # warm durable field or retired Session-update overlay enters that memo;
    # C3 a reducer reads the warm cache;
    # C4 a new split module is added below the owner directory; C5 an alias in
    # the publisher file reads the cache outside its CAS function; C6 the
    # candidate reads owner scope from a warm map. Effects: E1 C1 passes;
    # E2 C2/C3/C5/C6 fail; E3 C4 is scanned immediately. Rules:
    # P1=C1=>E1; P2=C1+C2=>E2; P3=C1+C3=>E2; P4=C1+C4+C3=>E3+E2;
    # P5=C1+C5=>E2 even though that file contains a legal publisher;
    # P6=C1+C6=>E2.
    # Constraint: within-pass `record` reads are legal only because the typed
    # CommittedPrefixBase is constructed from the durable Session root.
    entry = """
struct CommittedPrefixBase {
    owner_scope: String,
    record: SessionRecord,
    memo: ProjectionDeliveryMemo,
}
fn committed_prefix_base() {
    rehydrated_session_for(); SessionRecord::new();
}
fn build_committed_projection_candidate() {
    read_persisted_session_projection(session_id); record.into_delivery_memo();
}
fn build_committed_projection_candidate_over(base: CommittedPrefixBase) {}
fn plan() { awaken_managed_session_events::derive_coordination_events(); }
"""
    record = """
struct ProjectionDeliveryMemo {
    cache_revision: u64,
    transient_events: Vec<Event>,
    transient_owners: HashMap<String, String>,
}
fn into_delivery_memo(self) {
    let Self { cache_revision, events, checkpoint } = self;
}
"""
    assert projection_input_boundary_violations(entry, record, {}) == [], "P1/E1"
    for forbidden in ("durable_events: Vec<Event>", "overlay: ManagedLiveOverlay"):
        widened = record.replace(
            "    cache_revision: u64,",
            "    cache_revision: u64,\n    " + forbidden + ",",
        )
        assert projection_input_boundary_violations(entry, widened, {}), "P2/E2 " + forbidden
    raw = entry.replace("base: CommittedPrefixBase", "base: SessionRecord")
    assert projection_input_boundary_violations(raw, record, {}), "P3/E2 raw reducer"
    leak = {"future.rs": "fn reduce(&self) { self.sessions.lock().unwrap(); }"}
    assert projection_input_boundary_violations(entry, record, leak), "P3/E2 warm read"
    publisher = {
        "publisher.rs": "fn publish_projection_candidate(&self) { self.sessions.lock(); }"
    }
    assert projection_input_boundary_violations(entry, record, publisher) == [], "P1/E1 CAS owner"
    alias_leak = {
        "publisher.rs": publisher["publisher.rs"]
        + " fn reduce(shared: &ManagedState) { let alias = &shared.sessions; alias.lock(); }"
    }
    assert projection_input_boundary_violations(entry, record, alias_leak), "P5/E2 alias"
    child_status_leak = {
        "lifecycle_projection.rs": "fn plan_child_activity_projection(record: &SessionRecord) { record.child_threads.iter(); }"
    }
    assert projection_input_boundary_violations(
        entry, record, {}, child_status_leak
    ), "P7/E2 child output read"
    child_wire_write = {
        "lifecycle_projection.rs": "fn append_delegation_projections(record: &mut SessionRecord) { record.child_threads.push(child); }"
    }
    assert projection_input_boundary_violations(
        entry, record, {}, child_wire_write
    ) == [], "P7/E1 output write"
    # Causes: P8 a sibling renderer feeds its emitted tool events back into
    # domain selection; P9 the candidate bypasses the single domain plan.
    # Effects: E2 each case is rejected. Decision rows R8=P8=>E2,
    # R9=P9=>E2; ordinary wire writes still satisfy P7/E1.
    rendered_feedback = {
        "timeline_encoding.rs": "fn issued_tool_events(record: &SessionRecord) { record.events.iter(); }"
    }
    assert projection_input_boundary_violations(
        entry, record, {}, rendered_feedback
    ), "P8/E2 rendered feedback"
    assert projection_input_boundary_violations(
        entry.replace("awaken_managed_session_events::derive_coordination_events()", "reuse_rendered_events()"),
        record, {},
    ), "P9/E2 missing domain plan"
    # C10 selection is delegated to the existing evidence owner; C11 that
    # owner is missing or selects twice; C12 direct and delegated paths coexist.
    # P10 C10 alone => pass; P11 C11/C12 => reject. The move keeps one selector
    # and cannot turn a helper name alone into proof of a committed-prefix plan.
    delegated_entry = entry.replace("awaken_managed_session_events::derive_coordination_events()", "evidence.derive_plans()")
    evidence_owner = {"candidate_evidence.rs": "fn derive_plans() { awaken_managed_session_events::derive_coordination_events(); }"}
    assert projection_input_boundary_violations(delegated_entry, record, evidence_owner) == [], "P10"
    assert projection_input_boundary_violations(delegated_entry, record, {}), "P11 missing owner"
    duplicate_owner = {"candidate_evidence.rs": evidence_owner["candidate_evidence.rs"].replace(
        "derive_coordination_events();", "derive_coordination_events(); awaken_managed_session_events::derive_coordination_events();")}
    assert projection_input_boundary_violations(delegated_entry, record, duplicate_owner), "P11 duplicate selector"
    assert projection_input_boundary_violations(entry + " fn extra() { evidence.derive_plans(); }", record, evidence_owner), "P11 parallel paths"
    assert projection_input_boundary_violations(
        entry + " fn late(&self) { self.owner_scope(session_id); }", record, {}
    ), "P6/E2 warm owner"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        module = root / "crates/server/awaken-protocol-managed/src/state/events/committed_projection.rs"
        module.parent.mkdir(parents=True)
        module.write_text(entry, encoding="utf-8")
        (module.parent.parent / "session_record.rs").write_text(record, encoding="utf-8")
        subtree = module.with_suffix("")
        subtree.mkdir()
        (subtree / "future.rs").write_text(leak["future.rs"], encoding="utf-8")
        assert check_projection_input_boundary(root), "P4/E3 newly split reducer included"
