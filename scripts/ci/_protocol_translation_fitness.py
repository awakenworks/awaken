"""Protocol adapters translate; they never decide (ADR-0091 protocol boundary).

A protocol crate (Cargo metadata ``separation = "protocol"``) lowers wire
requests into application commands and projects application results onto its
wire. Session lifecycle, Resource manifest, idempotency and admission
decisions belong to the Session application and its aggregate. The compiler
cannot see an adapter re-deriving such a decision from raw domain state, so
this check refuses the lexical forms each decision needs:

| Rule | Production protocol source ...                                   | Why it is a decision |
|---|---|---|
| T1 | names a canonical Core persistence SPI or the App store composition role (including same-origin re-exports) | it reaches persistence past the application |
| T2 | constructs durable write vocabulary (`IdempotencyRecord {`,      | it authors a root mutation, a replay identity or a lifecycle fact |
|    | `SessionMutation {`, `SessionMutationPayload::`, `SessionTombstone {`, `ManagedLifecycleFact {`) | |
| T3 | calls an aggregate decision function (`transition_environment`,  | it evaluates the state machine itself |
|    | `transition_execution`, `realization_lease_*`, `realization_generation_authorizes`) | |
| T4 | tests a raw lifecycle enum variant (`SessionExecutionState::`,    | it selects a command from domain state instead of asking the |
|    | `ActivationState::`, `SessionEnvironmentPhase::`, `McpAttachmentState::` | domain or application for the named decision |
|    | in `==`, `!=`, `matches!` or a match arm) or reads `mutation_policy` | |
| T5 | edits a Resource manifest (`.attach(`, `.detach(`,                | it lowers, reuses or allocates Resource identities |
|    | `ResolvedSessionResources::try_new`, `SessionInputResolver::`, `input_binding(`) | |

Reading the aggregate through a domain-named predicate (``is_terminal()``,
``public_status()``) and converting a variant to its wire spelling
(``SessionExecutionState::X.as_str()``) remain projection. Test code
(``cfg(test)``, test files, test support) is not production and is exempt.
The ratchet is zero: there is no baseline.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace
from _contract_map_fitness import CanonicalOrigins

TEST_FILES = re.compile(r"(^|/)(tests?|test_support)(\.rs|/)|_tests\.rs$")
SESSION_CONTRACT = "awaken-session-contract"
PERSISTENCE_ORIGINS = {
    SESSION_CONTRACT: {"SessionRepository", "SessionRepositoryReads", "SessionReconciliation", "LifecycleOutbox"},
    "awaken-session-application": {"SessionStore"},
}

LIFECYCLE_ENUMS = r"(?:SessionExecutionState|ActivationState|SessionEnvironmentPhase|McpAttachmentState)"
VARIANT = LIFECYCLE_ENUMS + r"\s*::\s*[A-Z]\w*"
RULES: tuple[tuple[str, str, re.Pattern[str]], ...] = (
    (
        "T2",
        "constructs durable write vocabulary; the application authors mutations, replay identities and facts",
        re.compile(
            r"(?<!> )(?<!for )\b(?:IdempotencyRecord|SessionMutation|SessionTombstone|ManagedLifecycleFact)\s*\{"
            r"|\bSessionMutationPayload\s*::"
        ),
    ),
    (
        "T3",
        "evaluates the Session state machine; the aggregate decides",
        re.compile(
            r"\b(?:transition_environment|transition_execution|realization_generation_authorizes"
            r"|realization_lease_\w+)\b"
        ),
    ),
    (
        "T4",
        "selects behaviour from raw domain state; ask the application for the named decision",
        re.compile(
            r"(?:==|!=)\s*(?:\w+::)*" + VARIANT
            + r"|" + VARIANT + r"\s*(?:==|!=|=>|\|)"
            + r"|matches!\s*\([^;]*?" + VARIANT
            + r"|\.mutation_policy\b(?!\s*\()"
        ),
    ),
    (
        "T5",
        "edits a Resource manifest; the application lowers and allocates Resource identities",
        re.compile(
            r"\.(?:attach|detach)\s*\(|\bResolvedSessionResources\s*::\s*try_new\b"
            r"|\bSessionInputResolver\s*::|\binput_binding\s*\("
        ),
    ),
)


def source_violations(sources: dict[str, str], *, origins=None, repo_root=None) -> list[str]:
    """Rule violations in one protocol crate's compiled sources.

    Cause/effect graph: C1 the file is production protocol code; C2 its
    executable text (comments and strings removed) matches rule Tn. E1 accept
    when C1 is false or no rule matches; E2 reject once per (file, line, rule).
    """
    errors: list[str] = []
    origins = CanonicalOrigins(PERSISTENCE_ORIGINS) if origins is None else origins
    for relative, source in sorted(sources.items()):
        if TEST_FILES.search(relative):
            continue
        syntax = _crate_boundary_workspace.rust_syntax(
            _crate_boundary_workspace.production_rust(source)
        )
        reaches = (origins.file_reaches(Path(repo_root) / relative) if repo_root is not None
                   else origins.source_reaches(_crate_boundary_workspace.production_rust(source)))
        lines = sorted({line for line, _canonical_origin in reaches})
        errors.extend(f"{relative}:{line}: protocol T1: reaches the Session persistence port; call the application"
                      for line in lines)
        for rule, reason, pattern in RULES:
            lines = sorted({syntax.count("\n", 0, m.start()) + 1 for m in pattern.finditer(syntax)})
            errors.extend(f"{relative}:{line}: protocol {rule}: {reason}" for line in lines)
    return errors


def protocol_manifests(repo_root: Path) -> list[Path]:
    crates = repo_root / "crates"
    return [
        manifest
        for manifest in _crate_boundary_workspace.crate_manifests(crates)
        if _crate_boundary_workspace.load_manifest(manifest)
        .get("package", {})
        .get("metadata", {})
        .get("awaken", {})
        .get("separation")
        == "protocol"
    ]


def selftest() -> None:
    """Decision table for the protocol translation rules.

    | Row | Source                                                        | Effect |
    |---|---|---|
    | A1 | wire DTO lowering into an application command                 | accept |
    | A2 | `public_status()` / `is_terminal()` / `Variant.as_str()` reads | accept |
    | A3 | a forbidden form in cfg(test), a test file, a comment, a string | accept |
    | A4 | a return/impl type naming write vocabulary; a wire `mutation_policy()` lowering | accept |
    | R1 | `use awaken_session_application::SessionStore`        | reject T1 |
    | R2 | `IdempotencyRecord { key, payload_hash }`                     | reject T2 |
    | R3 | `ManagedLifecycleFact { .. }` / `SessionMutationPayload::Replace` | reject T2 |
    | R4 | `transition_environment(facts, event)`                         | reject T3 |
    | R5 | `match state { SessionExecutionState::Running => .. }`        | reject T4 |
    | R6 | `if execution == SessionExecutionState::Idle`                 | reject T4 |
    | R7 | `matches!(execution, SessionExecutionState::Preparing)`       | reject T4 |
    | R8 | `baseline.mutation_policy.is_managed()`                        | reject T4 |
    | R9 | `current.attach(input)` / `input_binding(id, &request, None)`  | reject T5 |
    | M1 | Core DTO/errors or App reconciliation report                  | accept |
    | M2 | Core persistence origins, aliases or a root glob              | reject T1 |
    """
    path = "crates/server/awaken-protocol-x/src/state.rs"
    accepted = (
        "fn lower(body: Body) -> Command { Command { inputs: body.inputs.iter().map(to_request).collect() } }\n"
        "fn project(s: &PersistedSession) -> Status { if s.is_terminal() { return Status::Done; }\n"
        "  match s.public_status() { SessionPublicStatus::Idle => Status::Idle, _ => Status::Busy } }\n"
        "fn spell() -> &'static str { SessionExecutionState::ActivationFailed.as_str() }\n"
    )
    assert source_violations({path: accepted}) == [], "A1/A2"
    signatures = (
        "fn fact() -> ManagedLifecycleFact { make() }\n"
        "impl Display for IdempotencyRecord { }\n"
        "fn lower(mode: Mode) -> Policy { mode.mutation_policy() }\n"
    )
    assert source_violations({path: signatures}) == [], "A4 types and wire lowering"
    forbidden = "fn f() { let r = IdempotencyRecord { key, payload_hash }; current.attach(input); }\n"
    assert source_violations({path: "#[cfg(test)]\nmod tests {\n" + forbidden + "}\n"}) == [], "A3 cfg"
    assert source_violations({"crates/server/awaken-protocol-x/src/state/tests.rs": forbidden}) == [], "A3 file"
    assert source_violations({"crates/server/awaken-protocol-x/src/test_support.rs": forbidden}) == [], "A3 support"
    assert source_violations(
        {path: "// IdempotencyRecord { key } current.attach(x)\nfn f() { let _ = \"IdempotencyRecord { k }\"; }\n"}
    ) == [], "A3 prose"
    rejected = {
        "R1": ("T1", "use awaken_session_application::SessionStore;\n"),
        "R2": ("T2", "fn f() { let r = IdempotencyRecord { key: k, payload_hash: h }; }\n"),
        "R3 fact": ("T2", "fn f() -> ManagedLifecycleFact { ManagedLifecycleFact { id } }\n"),
        "R3 payload": ("T2", "fn f() { let p = SessionMutationPayload::Replace(s); }\n"),
        "R4": ("T3", "fn f() { let d = transition_environment(facts, event); }\n"),
        "R5": ("T4", "fn f() { match state { SessionExecutionState::Running => a(), _ => b() } }\n"),
        "R6": ("T4", "fn f() { if execution == SessionExecutionState::Idle { a() } }\n"),
        "R7": ("T4", "fn f() { if matches!(execution, SessionExecutionState::Preparing) { a() } }\n"),
        "R8": ("T4", "fn f() { if baseline.mutation_policy.is_managed() { a() } }\n"),
        "R9 attach": ("T5", "fn f() { let d = current.attach(input); }\n"),
        "R9 binding": ("T5", "fn f() { let b = input_binding(id, &request, None); }\n"),
    }
    for row, (rule, source) in rejected.items():
        found = source_violations({path: source})
        assert len(found) == 1 and f"protocol {rule}:" in found[0], (row, found)
    # M1-M5 keep the normal/dev/build/target and inherited-alias distinctions,
    # now through the canonical symbol origins of the combined values/SPI owner.
    # Core DTO/errors and the similarly named App report are legitimate values.
    ordinary = "use awaken_session_contract::{SessionEventBatch, SessionRepositoryError};\n" \
               "use awaken_session_application::SessionReconciliation;\n"
    assert source_violations({path: ordinary}) == [], "M1 Core values/App report"
    for port in PERSISTENCE_ORIGINS[SESSION_CONTRACT]:
        assert source_violations({path: f"use awaken_session_contract::{{{port} as Port}};"}), "M2 " + port
    assert source_violations({path: "use awaken_session_contract::*;"}), "M2 root glob reaches SPI"
    assert source_violations({path: "use awaken_session_contract as values; fn f(_: &dyn values::SessionRepository) {}"}), "M2 namespace rename"
    _selftest_origin_reexports()



def _selftest_origin_reexports() -> None:
    """M3-M7: preserve SPI rejection through each actual origin route.

    Causes: dependency alias/direct/group/namespace/glob imports; inline,
    #[path] and literal-include module routes; two same-origin re-export hops;
    normal/build/dev and target declaration forms. Effects: all persistence
    origins reject; unrelated same-short-name App reports/value DTOs accept;
    test-only branches grant no production route. Fixtures run no Cargo.
    """
    import tempfile
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        def crate(name, source, extra=""):
            base = root / "crates" / name
            (base / "src").mkdir(parents=True)
            (base / "Cargo.toml").write_text('[package]\nname = "' + name + '"\n' + extra)
            (base / "src/lib.rs").write_text(source)
            return base
        core = crate(SESSION_CONTRACT, "pub struct SessionEventBatch; pub struct SessionRepositoryError;")
        app = crate("awaken-session-application", "pub struct SessionReconciliation;")
        first = crate("first", "pub mod inside { pub use origin::{SessionRepositoryReads as Reader}; }\n"
                      "pub use self::inside::Reader as Public;\n",
                      '[dependencies]\norigin = { package = "awaken-session-contract", version = "1" }\n')
        second = crate("second", 'include!("exports.rs");\n',
                       '[dependencies]\nfirst = { version = "1" }\n')
        (second / "src/exports.rs").write_text("pub use first::{Public as Observation};\n")
        selected = crate("selected", '#[path = "picked.rs"] pub mod view; pub use self::view::*;',
                         '[dependencies]\nsecond = { version = "1" }\n')
        (selected / "src/picked.rs").write_text("pub use second::Observation as ReadView;\n")
        protocol = crate("protocol", "", '[dependencies]\nselected = { version = "1" }\n')
        manifests = _crate_boundary_workspace.crate_manifests(root / "crates")
        origins = CanonicalOrigins(PERSISTENCE_ORIGINS, manifests)
        context = origins.libs["protocol"]
        for source in ["use selected::ReadView;", "use selected::{ReadView as R};",
                       "use selected::*;", "fn f(_: &dyn selected::ReadView) {}",
                       "use selected as x; fn f(_: &dyn x::ReadView) {}"]:
            assert origins.source_reaches(source, context), ("M3 transitive", source)
        assert not origins.source_reaches("use awaken_session_application::SessionReconciliation;"), "M4 App report is not Core SPI"
        assert not origins.source_reaches("use awaken_session_contract::{SessionEventBatch, SessionRepositoryError};"), "M4 Core values"
        # The canonical walker must keep the exact original files while exposing
        # namespace views; no independent module parser owns this origin graph.
        views = []
        original = _crate_boundary_workspace.production_module_files(manifests)
        with_views = _crate_boundary_workspace.production_module_files(manifests, module_views=views)
        assert original == with_views and any(view[2] == ("inside",) for view in views), "M5 one module walk"
        assert any(view[3].name == "exports.rs" and view[2] == () for view in views), "M5 include same namespace"
        for section in ("dependencies", "build-dependencies", "dev-dependencies"):
            for target in (False, True):
                for inherited in (False, True):
                    table = ('[target.\'cfg(windows)\'.' + section + ']\n' if target
                             else '[' + section + ']\n')
                    authored = '{ workspace = true }' if inherited else '{ package = "awaken-session-contract", version = "1" }'
                    (protocol / "Cargo.toml").write_text('[package]\nname = "protocol"\n' + table + 'named = ' + authored + '\n')
                    index = CanonicalOrigins(PERSISTENCE_ORIGINS, manifests,
                                             workspace_dependencies={"named": {"package": SESSION_CONTRACT}})
                    assert index.source_reaches("use named::{LifecycleOutbox as Events};", index.libs["protocol"]), \
                        ("M6 alias identity", section, target, inherited)
        test_only = crate("test-only", '#[cfg(test)] mod private { pub use origin::SessionRepository; }',
                          '[dependencies]\norigin = { package = "awaken-session-contract", version = "1" }\n')
        index = CanonicalOrigins(PERSISTENCE_ORIGINS,
                                 _crate_boundary_workspace.crate_manifests(root / "crates"),
                                 workspace_dependencies={"named": {"package": SESSION_CONTRACT}})
        assert not index.exported_symbols(index.libs["test-only"]), "M7 test-only route excluded"
        # M8 a local module shadow cannot capture an absolute external path.
        # The non-absolute same-short-name value is deliberately not the SPI.
        (protocol / "Cargo.toml").write_text('[package]\nname = "protocol"\n[dependencies]\nawaken-session-contract = { version = "1" }\n')
        shadow = "mod awaken_session_contract { pub struct SessionRepositoryReads; }\n"
        (protocol / "src/lib.rs").write_text(shadow + "fn f(_: &dyn ::awaken_session_contract::SessionRepositoryReads) {}")
        manifests = _crate_boundary_workspace.crate_manifests(root / "crates")
        index = CanonicalOrigins(PERSISTENCE_ORIGINS, manifests)
        assert index.file_reaches(protocol / "src/lib.rs"), "M8 absolute external SPI"
        assert not index.source_reaches("fn f(_: awaken_session_contract::SessionRepositoryReads) {}", index.libs["protocol"]), "M8 local value shadow"
        assert index.source_reaches("use ::awaken_session_contract::{SessionRepositoryReads as R};", index.libs["protocol"]), "M8 absolute grouped import"
        # M9 uses the SAME walker uncertainty collector. Missing/nonliteral
        # reachable inputs cannot become a green negative classification.
        for include in ['include!(concat!(env!("OUT_DIR"), "/unknown.rs"));', 'include!("missing.rs");']:
            (protocol / "src/lib.rs").write_text(include)
            index = CanonicalOrigins(PERSISTENCE_ORIGINS, manifests)
            assert index.uncertain_reachable("protocol"), "M9 unknown protocol input"
        (protocol / "src/lib.rs").write_text('include!("known.rs");')
        (protocol / "src/known.rs").write_text("use awaken_session_contract::SessionEventBatch;")
        unrelated = crate("unrelated-startup", 'include!(concat!(env!("OUT_DIR"), "/assets.rs"));')
        index = CanonicalOrigins(PERSISTENCE_ORIGINS, _crate_boundary_workspace.crate_manifests(root / "crates"))
        assert not index.uncertain_reachable("protocol"), "M9 known DTO-only include"
        assert not index.file_reaches(protocol / "src/known.rs"), "M9 value-only input has no SPI"
        assert index.uncertain_reachable("unrelated-startup"), "M9 unrelated generator stays separate"
        # M10 dependency uncertainty is also reachable even when protocol's own
        # source is a known DTO-only module. A normal alias edge carries the
        # unknown input; a disconnected startup generator does not.
        uncertain = crate("uncertain-adapter", 'include!("missing-generated.rs");')
        with (protocol / "Cargo.toml").open("a") as manifest:
            manifest.write('adapter = { package = "uncertain-adapter", version = "1" }\n')
        index = CanonicalOrigins(PERSISTENCE_ORIGINS, _crate_boundary_workspace.crate_manifests(root / "crates"))
        assert index.uncertain_reachable("protocol"), "M10 aliased normal dependency input"



def check_all(repo_root: Path) -> list[str]:
    root = repo_root.resolve()
    errors: list[str] = []
    manifests = protocol_manifests(root)
    if not manifests:
        return ["protocol translation: no crate declares separation = \"protocol\""]
    all_manifests = _crate_boundary_workspace.crate_manifests(root / "crates")
    workspace = _crate_boundary_workspace.load_manifest(root / "Cargo.toml").get("workspace", {})
    origins = CanonicalOrigins(PERSISTENCE_ORIGINS, all_manifests,
                               workspace_dependencies=workspace.get("dependencies", {}))
    for manifest in manifests:
        package = _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(manifest))
        errors.extend(f"{manifest.relative_to(root)}: protocol T1: unresolved compiled input; no negative SPI verdict: {unknown}"
                      for unknown in origins.uncertain_reachable(package))
        sources = {
            path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
            for path in _crate_boundary_workspace.production_module_files([manifest])
        }
        errors.extend(source_violations(sources, origins=origins, repo_root=root))
    return errors


if __name__ == "__main__":
    import sys

    selftest()
    found = check_all(_crate_boundary_workspace.REPO_ROOT)
    print("\n".join(found) if found else "OK - protocol adapters only translate")
    sys.exit(1 if found else 0)
