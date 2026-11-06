"""Dispatch hand-off stays apart from the Session executor and Worker-side effects.

Plan v4 D4 and section 3.3, principle P4 (dependencies point to the more
stable side). Dispatch owns the hand-off of claimed work: worker resolution,
placement, claim-failure mapping, the envelope codec, cancellation, the
terminal scan and the pool. It reaches the Session realization only through
the role traits its resolver is constructed with, and plugins declare their
auxiliary Agents through runtime-contract. The compiler cannot see a
dependency edge come back, so three rules hold it:

* D1 ``awaken-dispatch-execution`` has no normal dependency on
  ``awaken-session-realization`` or on any ``awaken-ext-*`` crate.
* D2 The Coordinator's dispatch path -- ``awaken-dispatch-execution``,
  ``awaken-dispatch`` (the Coordinator's dispatch adapter) and
  ``awaken-dispatch-http`` -- has no Worker-side implementation crate in its
  normal dependency closure.
* D3 The Coordinator ``awaken-coordinator`` has neither the Session executor
  (``awaken-session-executor``) nor any Worker-side implementation crate in
  its normal dependency closure. It consumes the executor through the
  execution ports a composition root selects; the root that is both
  Coordinator and Worker links the executor and realization, the Coordinator
  crate never does.
* D4 The Coordinator product binary (``awaken-coordinator``) is checked over
  the normal dependency closure of the package that declares it, because Cargo
  resolves dependencies per package: every target of that package links that
  closure. The closure reaches no Worker-side crate and not the Session
  executor; any reach is refused. The binary is declared by
  ``awaken-coordinator-process`` over the shared process startup
  (``awaken-process-startup``), which links no Worker (see
  ``docs/design/runtime-interface-boundaries.md#coordinator-only-composition``).
  The reviewed-transition baseline this rule held while the startup still
  linked the executor reached zero and was removed (plan v5 S2).

Why this is the correct check: the section 3.3 constraint -- no Worker-side
implementation anywhere in the Coordinator's closure -- is a property of the
dependency graph, which the compiler cannot see from inside one crate. D2
holds it for the dispatch path this boundary owns, the edge slice E
introduced (Coordinator -> run-ingress -> dispatch-execution ->
realization); D3 holds it for the Coordinator itself. The executor is named
explicitly because it is the Worker's execution engine even where it reaches
the realization only through role traits.

A Worker-side implementation crate is ``awaken-session-realization``,
``awaken-credential-materializer``, an ``awaken-sandbox*`` crate that is
neither a contract nor a Control-plane store, or a Worker composition root (a
``bootstrap`` crate of the ``worker`` context). Optional dependencies count:
an enabled feature must not reopen the edge.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path

import _crate_boundary_workspace
import _crate_dependency_fitness

DISPATCH_EXECUTION = "awaken-dispatch-execution"
DISPATCH_FORBIDDEN = "awaken-session-realization"
DISPATCH_PATH: tuple[str, ...] = (
    "awaken-dispatch-execution",
    "awaken-dispatch",
    "awaken-dispatch-http",
)
COORDINATOR = "awaken-coordinator"

# The Worker's execution engine: never in the Coordinator's closure (D3).
SESSION_EXECUTOR = "awaken-session-executor"

# The Coordinator product binary whose package closure D4 checks.
COORDINATOR_BINARY = "awaken-coordinator"

Specs = Mapping[str, _crate_dependency_fitness.CrateSpec]


def is_worker_side(spec: _crate_dependency_fitness.CrateSpec) -> bool:
    """A Worker-side implementation crate (see the module docs)."""
    if spec.name in ("awaken-session-realization", "awaken-credential-materializer"):
        return True
    if spec.context == "worker" and spec.layer == "bootstrap":
        return True
    return (
        spec.name.startswith("awaken-sandbox")
        and spec.layer != "contract"
        and spec.context != "control"
    )


def closure(specs: Specs, root: str) -> set[str]:
    """The workspace crates reachable from ``root`` by normal dependencies."""
    seen: set[str] = set()
    pending = [root]
    while pending:
        for dependency in specs[pending.pop()].normal_deps:
            if dependency in specs and dependency not in seen:
                seen.add(dependency)
                pending.append(dependency)
    return seen


def dispatch_boundary_errors(specs: Specs) -> list[str]:
    """Violations of D1-D3.

    Cause/effect graph: C1 dispatch-execution names realization or an
    extension directly; C2 a dispatch-path crate reaches a Worker-side
    implementation; C3 the Coordinator reaches a Worker-side implementation
    or the Session executor. E1 accept when none holds; E2 reject naming the
    crate and the rule.
    """
    errors: list[str] = []
    if DISPATCH_EXECUTION in specs:
        for dependency in sorted(specs[DISPATCH_EXECUTION].normal_deps):
            if dependency == DISPATCH_FORBIDDEN or dependency.startswith("awaken-ext-"):
                errors.append(
                    f"dispatch-boundary: {DISPATCH_EXECUTION} depends on {dependency}; "
                    "dispatch reaches the realization through its resolver's role traits "
                    "and plugins declare auxiliary Agents through runtime-contract (D1)"
                )
    for crate in DISPATCH_PATH:
        if crate not in specs:
            continue
        reached = sorted(name for name in closure(specs, crate) if is_worker_side(specs[name]))
        if reached:
            errors.append(
                f"dispatch-boundary: the Coordinator dispatch path crate {crate} reaches "
                f"Worker-side implementation {', '.join(reached)} (D2)"
            )
    if COORDINATOR in specs:
        reached = sorted(
            name
            for name in closure(specs, COORDINATOR)
            if name == SESSION_EXECUTOR or is_worker_side(specs[name])
        )
        if reached:
            errors.append(
                f"dispatch-boundary: {COORDINATOR} reaches {', '.join(reached)}; the "
                "Coordinator composes dispatch and consumes the executor through the "
                "execution ports its composition root selects (D3)"
            )
    return errors


def binary_package(binaries: Mapping[str, Iterable[str]], binary: str) -> str | None:
    """The one package that declares ``binary`` as a target, if exactly one does."""
    owners = sorted(package for package, targets in binaries.items() if binary in targets)
    return owners[0] if len(owners) == 1 else None


def coordinator_binary_reach(specs: Specs, package: str) -> list[str]:
    """The Worker-side crates and the Session executor ``package`` links (D4)."""
    linked = closure(specs, package) | {package}
    return sorted(
        name for name in linked if name == SESSION_EXECUTOR or is_worker_side(specs[name])
    )


def coordinator_binary_errors(package: str | None, reached: Iterable[str]) -> list[str]:
    """Violations of D4.

    Cause/effect graph: C1 no single package declares the binary; C2 the
    package reaches a Worker-side crate or the executor. E1 accept when
    neither holds; E2 reject naming the crate.
    """
    if package is None:
        return [f"dispatch-boundary: no single package declares the {COORDINATOR_BINARY} binary, so its "
                "closure cannot be checked (D4)"]
    return [
        f"dispatch-boundary: the {COORDINATOR_BINARY} binary's package {package} reaches {name}; "
        "compose the Coordinator without it (D4)"
        for name in sorted(set(reached))
    ]


def workspace_binaries() -> dict[str, list[str]]:
    """Each workspace package and the binary targets it declares."""
    binaries: dict[str, list[str]] = {}
    for manifest_path in _crate_boundary_workspace.iter_crate_manifests():
        manifest = _crate_boundary_workspace.load_manifest(manifest_path)
        binaries[_crate_boundary_workspace.package_name(manifest)] = [
            str(target["name"]) for target in manifest.get("bin", []) if "name" in target
        ]
    return binaries


def check_coordinator_binary() -> list[str]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    package = binary_package(workspace_binaries(), COORDINATOR_BINARY)
    return coordinator_binary_errors(package, coordinator_binary_reach(specs, package) if package else [])


def _specs(rows: Iterable[tuple[str, str, str, set[str]]]) -> dict[str, _crate_dependency_fitness.CrateSpec]:
    return {
        name: _crate_dependency_fitness.CrateSpec(
            name=name,
            context=context,
            layer=layer,
            authority=name,
            normal_deps=frozenset(deps),
        )
        for name, context, layer, deps in rows
    }


def selftest() -> None:
    """Decision table for the dispatch boundary.

    | Rule | Graph                                                        | Effect |
    |---|---|---|
    | B1 | dispatch-execution reaches only contracts; the Coordinator reaches neither the executor nor a Worker-side crate | accept |
    | B2 | dispatch-execution depends on awaken-session-realization     | reject D1 |
    | B3 | dispatch-execution depends on an awaken-ext-* crate          | reject D1 (and D2 only if Worker-side) |
    | B4 | run-ingress reaches a Worker-side Sandbox through a library  | reject D2 |
    | B5 | a Sandbox contract or a Control-plane Sandbox store reached  | accept (not Worker-side) |
    | B6 | the Coordinator depends on a Worker-side crate directly      | reject D3 |
    | B7 | the Coordinator reaches the executor, through a library      | reject D3 |
    | B8 | the Coordinator depends on the realization directly; its contract stays reachable | reject D3 naming only the realization |
    | B9 | the Coordinator binary's package reaches no Worker-side crate | accept |
    | B10 | that package reaches the executor through a Worker composition root | reject D4 naming each crate |
    | B13 | no package, or two packages, declare the binary | reject D4 |
    | B14 | the Coordinator crate depends on a Worker composition root | reject D3 |
    """
    selftest_acp_product_features()
    base = [
        ("awaken-dispatch-execution", "shared", "infrastructure", {"awaken-dispatch-contract"}),
        ("awaken-dispatch-contract", "shared", "contract", set()),
        ("awaken-dispatch", "coordinator", "infrastructure", {"awaken-dispatch-execution"}),
        ("awaken-dispatch-http", "coordinator", "interface", {"awaken-dispatch"}),
        ("awaken-session-realization", "shared", "infrastructure", {"awaken-sandbox-control"}),
        ("awaken-sandbox-control", "worker", "contract", set()),
        ("awaken-sandbox-local", "worker", "infrastructure", {"awaken-sandbox-control"}),
        ("awaken-sandbox-policy-store", "control", "infrastructure", set()),
        ("awaken-session-executor", "shared", "infrastructure", {"awaken-dispatch-contract"}),
        ("awaken-ext-memory", "runtime", "infrastructure", set()),
        ("awaken-session-application", "shared", "application", {"awaken-dispatch-contract"}),
        ("awaken-coordinator", "coordinator", "bootstrap", {"awaken-dispatch", "awaken-session-application"}),
        ("awaken-worker", "worker", "bootstrap", {"awaken-session-executor"}),
    ]

    def with_edge(name: str, *added: str) -> dict[str, _crate_dependency_fitness.CrateSpec]:
        rows = [(n, c, l, set(d) | (set(added) if n == name else set())) for n, c, l, d in base]
        return _specs(rows)

    assert dispatch_boundary_errors(_specs(base)) == [], "B1"
    b2 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-session-realization"))
    assert any("(D1)" in error for error in b2) and any("(D2)" in error for error in b2), "B2"
    b3 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-ext-memory"))
    assert [error for error in b3 if "(D1)" in error] and not [e for e in b3 if "(D2)" in e], "B3"
    b4 = dispatch_boundary_errors(with_edge("awaken-dispatch", "awaken-sandbox-local"))
    assert any("awaken-dispatch reaches Worker-side implementation awaken-sandbox-local" in e for e in b4), "B4"
    assert dispatch_boundary_errors(
        with_edge("awaken-dispatch", "awaken-sandbox-control", "awaken-sandbox-policy-store")
    ) == [], "B5"
    b6 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-sandbox-local"))
    assert any("awaken-coordinator reaches awaken-sandbox-local" in e and "(D3)" in e for e in b6), "B6"
    b7 = dispatch_boundary_errors(with_edge("awaken-session-application", "awaken-session-executor"))
    assert any("awaken-coordinator reaches awaken-session-executor" in e for e in b7), "B7"
    b8 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-session-realization"))
    assert [e for e in b8 if "(D3)" in e] == [
        "dispatch-boundary: awaken-coordinator reaches awaken-session-realization; the "
        "Coordinator composes dispatch and consumes the executor through the "
        "execution ports its composition root selects (D3)"
    ], "B8"
    b14 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-worker"))
    assert [e for e in b14 if "(D3)" in e] == [
        "dispatch-boundary: awaken-coordinator reaches awaken-session-executor, awaken-worker; the "
        "Coordinator composes dispatch and consumes the executor through the "
        "execution ports its composition root selects (D3)"
    ], "B14"
    # D4: C1 an existing single binary owner; C2 a Worker-side reach.
    # E1=C1+!C2 accepts a closed composition; E2=C1+C2 refuses each crate.
    assert coordinator_binary_errors("awaken-coordinator-process", []) == [], "B9 closed composition"
    for name in ["awaken-session-executor", "awaken-session-realization", "awaken-credential-materializer"]:
        errors = coordinator_binary_errors("awaken-coordinator-process", [name])
        assert len(errors) == 1 and name in errors[0], "B10 no reopened reach"
    product = _specs(base + [("awaken-cli", "apps", "bootstrap", {"awaken-coordinator", "awaken-worker"})])
    assert dispatch_boundary_errors(product) == [], "B10 the binary's package is outside D1-D3"
    reached = coordinator_binary_reach(product, "awaken-cli")
    assert reached == ["awaken-session-executor", "awaken-worker"], f"B10 reach {reached}"
    assert coordinator_binary_errors("awaken-cli", reached) == [
        "dispatch-boundary: the awaken-coordinator binary's package awaken-cli reaches awaken-session-executor; "
        "compose the Coordinator without it (D4)",
        "dispatch-boundary: the awaken-coordinator binary's package awaken-cli reaches awaken-worker; "
        "compose the Coordinator without it (D4)",
    ], "B10"
    assert binary_package({"awaken-cli": ["awaken", "awaken-coordinator"], "awaken-worker": ["awaken-worker"]},
                          "awaken-coordinator") == "awaken-cli", "B13 single owner"
    for binaries in ({"awaken-cli": ["awaken"]},
                     {"awaken-cli": ["awaken-coordinator"], "other": ["awaken-coordinator"]}):
        package = binary_package(binaries, "awaken-coordinator")
        assert package is None, "B13"
        assert "no single package" in coordinator_binary_errors(package, [])[0], "B13"


ACP_EXECUTOR = "awaken-run-executor-acp"
ACP_METADATA_PRODUCTS = frozenset({"awaken-coordinator-process", "awaken-control-process"})


def acp_product_feature_errors(compiler_output: str, root: Path) -> list[str]:
    """Check actual Cargo compiler-artifact features of the isolated product.

    The existing benchmark parser validates test executables, not check-only
    library artifacts. This boundary reads Cargo's standard JSON directly and
    never reconstructs feature resolution from source or dependency names.
    """
    manifest = (root / "crates/platform/execution/awaken-run-executor-acp/Cargo.toml").resolve()
    source = manifest.parent / "src/lib.rs"
    records = []
    finished = []
    try:
        for line in compiler_output.splitlines():
            if not line.startswith("{"):
                continue
            record = json.loads(line)
            if not isinstance(record, dict):
                raise ValueError("compiler message is not an object")
            if record.get("reason") == "build-finished":
                finished.append(record.get("success") is True)
            if record.get("reason") != "compiler-artifact":
                continue
            if Path(record.get("manifest_path", "")).resolve() != manifest:
                continue
            target = record.get("target", {})
            if (target.get("name") != "awaken_run_executor_acp"
                    or target.get("kind") != ["lib"]
                    or Path(target.get("src_path", "")).resolve() != source):
                continue
            features = record.get("features")
            if not isinstance(features, list) or not all(isinstance(value, str) for value in features):
                raise ValueError("canonical artifact has malformed features")
            if record.get("profile", {}).get("test") is not False:
                raise ValueError("canonical artifact is not a production library")
            records.append(set(features))
    except (ValueError, TypeError, AttributeError) as error:
        return ["dispatch-boundary: malformed ACP compiler evidence: " + str(error) + " (D4 ACP)"]
    if finished != [True]:
        return ["dispatch-boundary: one successful isolated product build is required (D4 ACP)"]
    if not records:
        return ["dispatch-boundary: missing canonical ACP catalog/blob compiler artifact (D4 ACP)"]
    active = sorted(set().union(*records))
    if active:
        return ["dispatch-boundary: isolated metadata/blob product compiled ACP implementation features "
                + ", ".join(active) + " (D4 ACP)"]
    return []


def selftest_acp_product_features() -> None:
    # Cause/effect table: P1 actual canonical production artifact + empty features
    # + successful build accepts; P2 execution/host/real-acp or unreviewed feature
    # rejects; P3 missing/malformed/wrong-owner/failed/incomplete evidence rejects;
    # P4 unrelated package features are irrelevant; P5 every compiled occurrence
    # must remain metadata-only. These synthetic JSON tests execute no Cargo.
    root = Path("/fixture")
    artifact = {"reason": "compiler-artifact", "package_id": "fixture-acp",
                "manifest_path": str(root / "crates/platform/execution/awaken-run-executor-acp/Cargo.toml"),
                "target": {"name": "awaken_run_executor_acp", "kind": ["lib"],
                           "src_path": str(root / "crates/platform/execution/awaken-run-executor-acp/src/lib.rs")},
                "profile": {"test": False}, "features": []}
    done = {"reason": "build-finished", "success": True}
    def output(*records):
        return "\n".join(json.dumps(record) for record in records)
    assert acp_product_feature_errors(output(artifact, done), root) == [], "P1"
    for feature in ["execution", "host-preparation", "real-acp", "future-role"]:
        assert acp_product_feature_errors(output({**artifact, "features": [feature]}, done), root), "P2 " + feature
    for rows in [[], [done], [artifact], [artifact, {**done, "success": False}],
                 [{**artifact, "features": "execution"}, done],
                 [{**artifact, "manifest_path": "/other/Cargo.toml"}, done],
                 [{**artifact, "profile": {"test": True}}, done]]:
        assert acp_product_feature_errors(output(*rows), root), "P3"
    assert acp_product_feature_errors("{bad json", root), "P3 malformed"
    other = {**artifact, "manifest_path": "/other/Cargo.toml", "features": ["execution"]}
    assert acp_product_feature_errors(output(other, artifact, done), root) == [], "P4"
    assert acp_product_feature_errors(output(artifact, artifact, done), root) == [], "P5 fresh/cached records"
    assert acp_product_feature_errors(output(artifact, {**artifact, "features": ["execution"]}, done), root), "P5 no hidden union"


def check_acp_product_features(package: str) -> int:
    """Explicit qualification entry point; normal source guards launch no Cargo."""
    selftest_acp_product_features()
    if package not in ACP_METADATA_PRODUCTS:
        print("dispatch-boundary: not an isolated ACP metadata product: " + package, file=sys.stderr)
        return 1
    root = Path(__file__).resolve().parents[2]
    command = [os.environ.get("CARGO", "cargo"), "check", "--locked", "--package", package,
               "--message-format=json"]
    result = subprocess.run(command, cwd=root, capture_output=True, text=True, check=False)
    if result.returncode:
        print(result.stdout, end="")
        print(result.stderr, file=sys.stderr, end="")
        return result.returncode
    errors = acp_product_feature_errors(result.stdout, root)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("dispatch-boundary: " + package + " compiled canonical catalog/blob custody without ACP implementation features (D4 ACP)")
    return 0


def check_all() -> list[str]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    return dispatch_boundary_errors(specs) + check_coordinator_binary()


def main(argv: list[str]) -> int:
    """No argument checks D1-D4; ``--self-test`` runs the decision table."""
    parser = argparse.ArgumentParser(prog=Path(__file__).name, description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true", help="run the decision table")
    modes.add_argument("--check-acp-product-features", metavar="PACKAGE",
                       help="build PACKAGE and refuse ACP implementation features in it")
    arguments = parser.parse_args(argv)
    if arguments.check_acp_product_features:
        return check_acp_product_features(arguments.check_acp_product_features)
    if arguments.self_test:
        selftest()
        print("_dispatch_boundary_fitness selftest: the decision table holds")
        return 0
    errors = check_all()
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if not errors:
        print("OK - the dispatch hand-off and the Coordinator binary link no Worker-side crate.")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
