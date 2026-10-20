"""Compiler conditions for the existing SDK qualifier, not a Cargo resolver."""
from __future__ import annotations

import hashlib
from pathlib import Path, PurePosixPath
import posixpath
import re
import tomllib

import formal_sources
from public_api_selection import selected_crates
from _crate_boundary_workspace import normal_dependency_names

ENVIRONMENT_KEY = "K8S_OPENAPI_ENABLED_VERSION"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def observe_source(root: Path, revision: str, members: dict, sdk: list[str]) -> dict:
    """Read only this selected source's manifests/lock through the Git owner.

    Normal/build edges include every optional/target declaration. External lock
    versions are unioned by name: absence in this conservative graph is useful;
    it is not an enabled-feature graph or an executed build observation.
    """
    hashes, manifests = {}, {}

    def read(path: str) -> dict:
        name = PurePosixPath(path)
        if name.is_absolute() or ".." in name.parts:
            raise ValueError("SDK compiler source path is outside its Git subject")
        data = formal_sources.git_output(root, "show", revision + ":" + path)
        hashes[path] = hashlib.sha256(data).hexdigest()
        return tomllib.loads(data.decode())

    workspace = read("Cargo.toml").get("workspace", {})
    locked = read("Cargo.lock")["package"]
    if not isinstance(locked, list):
        raise ValueError("SDK compiler source has no complete dependency lock")
    locks = {}
    for package in locked:
        if not isinstance(package.get("name"), str) or not isinstance(package.get("version"), str):
            raise ValueError("SDK compiler lock has an incomplete package identity")
        locks.setdefault(package["name"], []).append(package)
    selected = []
    pending = []
    for name in sdk:
        path = Path(members[name]["manifest_path"]).resolve().relative_to(root.resolve()).as_posix()
        document = read(path)
        package = document["package"]
        if package["name"] != name:
            raise ValueError("SDK compiler source differs from the metadata selection")
        selected.append({"name": name, "metadata": package.get("metadata"),
                         "publish": [] if package.get("publish") is False else package.get("publish")})
        manifests[path] = document
        pending.append(("manifest", path))
    if selected_crates(selected)[1] != sdk:
        raise ValueError("SDK compiler source does not contain the complete SDK selection")
    seen, external = set(), set()
    while pending:
        kind, identity = pending.pop()
        if (kind, identity) in seen:
            continue
        seen.add((kind, identity))
        if kind == "manifest":
            document = manifests[identity]
            for sections in [document, *document.get("target", {}).values()]:
                for alias in normal_dependency_names(sections):
                    for section in ("dependencies", "build-dependencies"):
                        if alias not in sections.get(section, {}):
                            continue
                        authored = sections[section][alias]
                        authored = {"version": authored} if isinstance(authored, str) else authored
                        inherited = workspace.get("dependencies", {}).get(alias) if authored.get("workspace") else {}
                        if inherited is None:
                            raise ValueError("SDK compiler source has an unresolved workspace dependency")
                        inherited = {"version": inherited} if isinstance(inherited, str) else inherited
                        dependency = {**inherited, **authored}
                        if "path" in dependency:
                            base = PurePosixPath() if authored.get("workspace") else PurePosixPath(identity).parent
                            path = posixpath.normpath(str(base / dependency["path"] / "Cargo.toml"))
                            if path not in manifests:
                                manifests[path] = read(path)
                            pending.append(("manifest", path))
                        else:
                            pending.append(("external", dependency.get("package", alias)))
        else:
            if identity not in locks or any(package.get("source") is None for package in locks[identity]):
                raise ValueError("SDK compiler source has an unresolved normal/build dependency: " + identity)
            external.add(identity)
            for package in locks[identity]:
                dependencies = package.get("dependencies", [])
                if not isinstance(dependencies, list) or any(not isinstance(dep, str) or not dep for dep in dependencies):
                    raise ValueError("SDK compiler lock has incomplete dependency edges")
                pending.extend(("external", dep.split(" ", 1)[0]) for dep in dependencies)
    packages = locks.get("k8s-openapi", []) if "k8s-openapi" in external else []
    return {"revision": revision, "sdk_crates": sdk, "source_hashes": hashes,
            "topology": "conservative normal/build, all optional/target edges, external locked-name union",
            "applicability": "applicable" if packages else "not_applicable", "model": None,
            "root_model_declaration": workspace.get("dependencies", {}).get("k8s-openapi"),
            "normal_build_k8s_packages": [{key: package.get(key) for key in ("name", "version", "source")} for package in packages]}


def model(source: dict, package: dict) -> dict:
    declaration = source["root_model_declaration"]
    features = declaration.get("features") if isinstance(declaration, dict) else None
    if (not isinstance(features, list) or not features or any(not isinstance(value, str) for value in features)
            or len(set(features)) != len(features)):
        raise ValueError("SDK compiler input requires one explicit workspace Kubernetes model")
    versions = [value for value in features if re.fullmatch(r"v1_[1-9][0-9]*", value)]
    supported = package.get("features")
    if (len(versions) != 1 or not isinstance(supported, dict)
            or any(value not in supported or supported[value] != [] for value in features)):
        raise ValueError("SDK compiler input model is multiple, indirect or unsupported by its registry package")
    identity = {key: package[key] for key in ("name", "version", "source")}
    if source["normal_build_k8s_packages"] != [identity]:
        raise ValueError("SDK compiler source lock and metadata Kubernetes identities differ")
    return {"feature": versions[0], "value": versions[0].removeprefix("v").replace("_", "."),
            "package": {key: package[key] for key in ("id", "name", "version", "source")},
            "declaration": "workspace.dependencies.k8s-openapi.features"}


def qualification(root: Path, revision: str, members: dict, sdk: list[str], metadata: dict,
                  values: dict, baseline_rev: str = "") -> dict:
    current = observe_source(root, revision, members, sdk)
    baseline = observe_source(root, baseline_rev, members, sdk) if baseline_rev else None
    result = {"current": current, "environment": {}, "ambient": "unused"}
    if baseline is not None:
        result["baseline"] = baseline
    if current["applicability"] == "not_applicable":
        if baseline is not None and baseline["applicability"] != "not_applicable":
            raise ValueError("SDK compiler input cannot borrow a model from the baseline")
        return result
    packages = [package for package in metadata["packages"] if package["name"] == "k8s-openapi"]
    if (len(packages) != 1 or packages[0].get("source") != REGISTRY
            or not isinstance(packages[0].get("id"), str) or not packages[0]["id"]
            or sum(node["id"] == packages[0]["id"] for node in metadata["resolve"]["nodes"]) != 1):
        raise ValueError("SDK compiler input requires a unique resolved registry Kubernetes package")
    current["model"] = model(current, packages[0])
    expected = current["model"]["value"]
    if baseline is not None:
        if baseline["applicability"] == "not_applicable":
            baseline["shared_environment"] = "inherited_unused"
        else:
            baseline["model"] = model(baseline, packages[0])
            if baseline["model"]["value"] != expected:
                raise ValueError("SDK compiler input models conflict between current and baseline")
            baseline["shared_environment"] = "model_used"
    if ENVIRONMENT_KEY in values and values[ENVIRONMENT_KEY] != expected:
        raise ValueError("SDK compiler input conflicts with ambient " + ENVIRONMENT_KEY)
    result["ambient"] = "matching" if ENVIRONMENT_KEY in values else "absent"
    result["environment"] = {ENVIRONMENT_KEY: expected}
    return result
