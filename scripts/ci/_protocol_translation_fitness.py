"""Protocol adapters translate; they never decide (plan v4 D7, principles P2/P4).

A protocol crate (Cargo metadata ``separation = "protocol"``) lowers wire
requests into application commands and projects application results onto its
wire. Session lifecycle, Resource manifest, idempotency and admission
decisions belong to the Session application and its aggregate. The compiler
cannot see an adapter re-deriving such a decision from raw domain state, so
this check refuses the lexical forms each decision needs:

| Rule | Production protocol source ...                                   | Why it is a decision |
|---|---|---|
| T1 | depends on, or names, `awaken-session-repository-contract`       | it reaches the persistence port past the application |
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

TEST_FILES = re.compile(r"(^|/)(tests?|test_support)(\.rs|/)|_tests\.rs$")
REPOSITORY_CONTRACT = "awaken-session-repository-contract"

LIFECYCLE_ENUMS = r"(?:SessionExecutionState|ActivationState|SessionEnvironmentPhase|McpAttachmentState)"
VARIANT = LIFECYCLE_ENUMS + r"\s*::\s*[A-Z]\w*"
RULES: tuple[tuple[str, str, re.Pattern[str]], ...] = (
    (
        "T1",
        "reaches the Session persistence port; call the application",
        re.compile(r"\bawaken_session_repository_contract\b"),
    ),
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


def source_violations(sources: dict[str, str]) -> list[str]:
    """Rule violations in one protocol crate's compiled sources.

    Cause/effect graph: C1 the file is production protocol code; C2 its
    executable text (comments and strings removed) matches rule Tn. E1 accept
    when C1 is false or no rule matches; E2 reject once per (file, line, rule).
    """
    errors: list[str] = []
    for relative, source in sorted(sources.items()):
        if TEST_FILES.search(relative):
            continue
        syntax = _crate_boundary_workspace.rust_syntax(
            _crate_boundary_workspace.production_rust(source)
        )
        for rule, reason, pattern in RULES:
            lines = sorted({syntax.count("\n", 0, m.start()) + 1 for m in pattern.finditer(syntax)})
            errors.extend(f"{relative}:{line}: protocol {rule}: {reason}" for line in lines)
    return errors


def manifest_violations(relative: str, manifest: dict) -> list[str]:
    """T1 at the dependency level: only dev-dependencies may name the port."""
    if REPOSITORY_CONTRACT in manifest.get("dependencies", {}):
        return [f"{relative}: protocol T1: {REPOSITORY_CONTRACT} is a production dependency"]
    return []


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
    | R1 | `use awaken_session_repository_contract::SessionStore`        | reject T1 |
    | R2 | `IdempotencyRecord { key, payload_hash }`                     | reject T2 |
    | R3 | `ManagedLifecycleFact { .. }` / `SessionMutationPayload::Replace` | reject T2 |
    | R4 | `transition_environment(facts, event)`                         | reject T3 |
    | R5 | `match state { SessionExecutionState::Running => .. }`        | reject T4 |
    | R6 | `if execution == SessionExecutionState::Idle`                 | reject T4 |
    | R7 | `matches!(execution, SessionExecutionState::Preparing)`       | reject T4 |
    | R8 | `baseline.mutation_policy.is_managed()`                        | reject T4 |
    | R9 | `current.attach(input)` / `input_binding(id, &request, None)`  | reject T5 |
    | M1 | repository contract under [dev-dependencies] only             | accept |
    | M2 | repository contract under [dependencies]                      | reject T1 |
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
        "R1": ("T1", "use awaken_session_repository_contract::SessionStore;\n"),
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
    dev_only = {"dependencies": {}, "dev-dependencies": {REPOSITORY_CONTRACT: {}}}
    assert manifest_violations("x/Cargo.toml", dev_only) == [], "M1"
    production = {"dependencies": {REPOSITORY_CONTRACT: {}}}
    assert len(manifest_violations("x/Cargo.toml", production)) == 1, "M2"


def check_all(repo_root: Path) -> list[str]:
    root = repo_root.resolve()
    errors: list[str] = []
    manifests = protocol_manifests(root)
    if not manifests:
        return ["protocol translation: no crate declares separation = \"protocol\""]
    for manifest in manifests:
        errors.extend(
            manifest_violations(
                manifest.relative_to(root).as_posix(),
                _crate_boundary_workspace.load_manifest(manifest),
            )
        )
        sources = {
            path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
            for path in _crate_boundary_workspace.production_module_files([manifest])
        }
        errors.extend(source_violations(sources))
    return errors


if __name__ == "__main__":
    import sys

    selftest()
    found = check_all(_crate_boundary_workspace.REPO_ROOT)
    print("\n".join(found) if found else "OK - protocol adapters only translate")
    sys.exit(1 if found else 0)
