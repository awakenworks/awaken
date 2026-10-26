"""A dependency on a workspace crate is declared where it is used.

The context map and every crate-graph ratchet read the declared dependency
graph. A `[dependencies]` entry on a workspace crate that production code never names
adds an edge nobody uses, and a ratchet seeded from the graph freezes it as an
accepted crossing. So for every normal dependency on a workspace crate:

* production code names it: it stays (the scan skips comments and `cfg(test)`
  code but keeps `cfg(kani)` code, which `cargo kani` builds from the normal
  dependencies);
* only test code names it (`cfg(test)`, `tests/`, `benches/`, `examples/`): it
  belongs in `[dev-dependencies]`;
* nothing names it: it is removed, unless the crate declares it only to turn on
  features of a crate it uses, which `metadata.awaken.enables` lists; such an
  entry must set `features` or be named by a `[features]` entry.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from pathlib import Path
from typing import NamedTuple

import _crate_boundary_workspace

_KANI_CFG = re.compile(r"\bkani\b")


class DependencyUse(NamedTuple):
    crate: str
    dependency: str
    production: bool
    tests: bool
    turns_on_features: bool


def dependency_use_errors(uses: Iterable[DependencyUse], enables: dict[str, frozenset[str]]) -> list[str]:
    errors: list[str] = []
    seen: dict[str, set[str]] = {}
    for use in sorted(uses):
        seen.setdefault(use.crate, set()).add(use.dependency)
        listed = use.dependency in enables.get(use.crate, frozenset())
        if use.production:
            if listed:
                errors.append(f"{use.crate} names {use.dependency} in production code; remove it from "
                              "metadata.awaken.enables")
        elif listed:
            if not use.turns_on_features:
                errors.append(f"{use.crate} lists {use.dependency} in metadata.awaken.enables but turns on "
                              "none of its features; remove the dependency")
        elif use.tests:
            errors.append(f"{use.crate} names {use.dependency} only in test code; move it to [dev-dependencies]")
        else:
            errors.append(f"{use.crate} never names {use.dependency}; remove the dependency, or list it in "
                          "metadata.awaken.enables if it is declared only to turn on features")
    for crate, listed in sorted(enables.items()):
        for dependency in sorted(listed - seen.get(crate, set())):
            errors.append(f"{crate} lists {dependency} in metadata.awaken.enables but does not depend on it")
    return [f"dependency-use: {error}" for error in errors]


def _names(sources: Iterable[str], dependency: str) -> bool:
    pattern = re.compile(rf"\b{re.escape(dependency.replace('-', '_'))}\b")
    return any(pattern.search(source) for source in sources)


def _production_source(text: str) -> str:
    # Keep `cfg(kani)` code: the projection drops it as non-shipped, but the
    # proofs it holds are built from the normal dependencies.
    return _crate_boundary_workspace.production_rust(_KANI_CFG.sub("kani_build", text))


def crate_uses(manifest_path: Path, members: frozenset[str]) -> tuple[list[DependencyUse], frozenset[str]]:
    manifest = _crate_boundary_workspace.load_manifest(manifest_path)
    crate = _crate_boundary_workspace.package_name(manifest)
    root = manifest_path.parent
    declared: dict[str, list[_crate_boundary_workspace.DeclaredDependency]] = {}
    for dependency in _crate_boundary_workspace.dependency_declarations(manifest, include_build=False):
        if dependency.package in members:
            declared.setdefault(dependency.package, []).append(dependency)
    rust = sorted(root.rglob("*.rs"))
    in_source = [path for path in rust if path.relative_to(root).parts[0] in {"src", "build.rs"}]
    in_tests = [path for path in rust if path.relative_to(root).parts[0] in {"tests", "benches", "examples"}]
    production = [_production_source(path.read_text(encoding="utf-8")) for path in in_source]
    whole = [path.read_text(encoding="utf-8") for path in in_source + in_tests]
    feature_refs = {
        entry.removeprefix("dep:").split("/")[0].rstrip("?")
        for entries in manifest.get("features", {}).values()
        for entry in entries
    }
    uses = [
        DependencyUse(
            crate,
            name,
            production=any(_names(production, dependency.alias) for dependency in declarations),
            tests=any(_names(whole, dependency.alias) for dependency in declarations),
            turns_on_features=any(dependency.specification.get("features")
                                  or dependency.alias in feature_refs for dependency in declarations),
        )
        for name, declarations in declared.items()
    ]
    metadata = manifest.get("package", {}).get("metadata", {}).get("awaken", {})
    return uses, frozenset(metadata.get("enables", ()))


def check_all(crates: Path) -> list[str]:
    uses: list[DependencyUse] = []
    enables: dict[str, frozenset[str]] = {}
    manifests = _crate_boundary_workspace.crate_manifests(crates)
    members = frozenset(
        _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path)) for path in manifests
    )
    for path in manifests:
        crate_use, listed = crate_uses(path, members)
        uses += crate_use
        if listed:
            enables[_crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path))] = listed
    return dependency_use_errors(uses, enables)


def selftest() -> None:
    """Decision table.

    | Rule | cause                                                     | effect |
    | D1   | named in production code                                  | accept |
    | D2   | named only in test code                                   | error: dev-dependency |
    | D3   | named nowhere, not in `enables`                           | error: remove |
    | D4   | named nowhere, in `enables`, turns on features / does not | accept / error |
    | D5   | in `enables` but named in production / not a dependency   | error / error |
    | D6   | `cfg(test)` use is test code; `cfg(kani)` use is production | as stated |
    """

    def use(dep: str, production: bool = False, tests: bool = False, features: bool = False) -> DependencyUse:
        return DependencyUse("c", dep, production, tests or production, features)

    assert dependency_use_errors([use("a", production=True)], {}) == [], "D1"
    assert "dev-dependencies" in dependency_use_errors([use("a", tests=True)], {})[0], "D2"
    assert "never names" in dependency_use_errors([use("a")], {})[0], "D3"
    listed = {"c": frozenset({"a"})}
    assert dependency_use_errors([use("a", features=True)], listed) == [], "D4"
    assert "turns on none" in dependency_use_errors([use("a")], listed)[0], "D4 no features"
    assert "remove it from" in dependency_use_errors([use("a", production=True)], listed)[0], "D5 used"
    assert "does not depend" in dependency_use_errors([], listed)[0], "D5 stale"

    source = "#[cfg(test)]\nmod tests { use awaken_t::X; }\n#[cfg(kani)]\nmod proofs { use awaken_k::Y; }\n"
    projected = _production_source(source)
    assert not _names([projected], "awaken-t") and _names([projected], "awaken-k"), "D6"
    # D7 actual Cargo aliases, including target-only aliases, are the Rust
    # namespaces to scan. Multiple declarations of one package produce one
    # owner row and union feature activation; dev/build-only entries remain
    # outside this normal-dependency usage policy. The graph owner still sees
    # build declarations through its default declaration selection.
    import tempfile
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "src").mkdir()
        manifest = root / "Cargo.toml"
        manifest.write_text('[package]\nname="fixture"\n'
                            '[dependencies]\nrenamed={package="awaken-t"}\n'
                            '[target.\'cfg(windows)\'.dependencies]\nother={package="awaken-t",features=["extra"]}\n'
                            '[build-dependencies]\nbuild_only={package="awaken-k"}\n'
                            '[dev-dependencies]\ndev_only={package="awaken-k"}\n')
        for text, production, tests in [
            ("use renamed::Thing;", True, True),
            ("use other::Thing;", True, True),
            ("#[cfg(test)]\nmod tests { use renamed::Thing; }", False, True),
            ("", False, False),
        ]:
            (root / "src/lib.rs").write_text(text)
            observed, listed = crate_uses(manifest, frozenset({"awaken-t", "awaken-k"}))
            assert not listed
            assert observed == [DependencyUse("fixture", "awaken-t", production, tests, True)], "D7"
    print("_dependency_use_fitness selftest: the decision table holds")
