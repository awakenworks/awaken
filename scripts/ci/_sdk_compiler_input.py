"""Compiler conditions for the existing SDK qualifier, not a Cargo resolver."""
from __future__ import annotations

import hashlib
from pathlib import Path, PurePosixPath
import posixpath
import re
import tempfile
import tomllib

import formal_sources
import _sdk_archives
import _crate_boundary_workspace as workspace
from public_api_selection import selected_crates
from _crate_boundary_workspace import dependency_declarations

ENVIRONMENT_KEY = "K8S_OPENAPI_ENABLED_VERSION"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def source_package(document: dict) -> dict:
    """Project authored identity into the existing metadata SDK selector."""
    package = document["package"]
    if not isinstance(package.get("name"), str) or not package["name"]:
        raise ValueError("SDK source has an incomplete package identity")
    return {"name": package["name"], "metadata": package.get("metadata"),
            "publish": [] if package.get("publish") is False else package.get("publish")}


def baseline_inventory(root: Path, revision: str) -> dict:
    """Discover names and historical paths from the baseline's complete source."""
    if "commit" in formal_sources.git_paths(
            root, "ls-tree", "-r", "--format=%(objecttype)", "-z", revision):
        raise ValueError("SDK baseline gitlinks cannot establish a complete source inventory")
    archive_bytes = formal_sources.git_output(root, "archive", "--format=tar", revision)
    with tempfile.TemporaryDirectory(prefix="sdk-baseline-inventory-") as temporary:
        directory = Path(temporary)
        archive = directory / "source.tar"
        archive.write_bytes(archive_bytes)
        source = directory / "source"
        source.mkdir()
        _sdk_archives.unpack_source_archive(archive, source)
        # Git archive applies export-ignore/export-subst (including local
        # info/attributes). Only the revision's complete Git manifest inventory
        # and exact compiler documents may authorize first-release classification.
        documents = [path for path in formal_sources.git_paths(
            root, "ls-tree", "-r", "--name-only", "-z", revision)
            if path in ("Cargo.toml", "Cargo.lock") or path.endswith("/Cargo.toml")]
        hashes = {}
        for path in documents:
            archived = source / path
            if not archived.is_file() or archived.is_symlink():
                raise ValueError("SDK baseline archive omits a historical compiler document: " + path)
            original = formal_sources.git_output(root, "show", revision + ":" + path)
            if archived.read_bytes() != original:
                raise ValueError("SDK baseline archive rewrites a historical compiler document: " + path)
            hashes[path] = hashlib.sha256(original).hexdigest()
        root_manifest = workspace.load_manifest(source / "Cargo.toml")
        patterns = root_manifest.get("workspace", {}).get("members")
        if (not isinstance(patterns, list) or not patterns
                or any(not isinstance(pattern, str) or not pattern for pattern in patterns)):
            raise ValueError("SDK baseline has no complete declared workspace")
        for pattern in patterns:
            if PurePosixPath(pattern).is_absolute() or ".." in PurePosixPath(pattern).parts:
                raise ValueError("SDK baseline workspace member escapes its source")
            if not any((path / "Cargo.toml").is_file() for path in source.glob(pattern)):
                raise ValueError("SDK baseline is missing a declared workspace member")
        members = workspace.workspace_member_dirs(source)
        if {path.parent.resolve() for path in workspace.crate_manifests(source / "crates")} != set(members):
            raise ValueError("SDK baseline manifest discovery differs from its workspace")
        packages, selected = {}, []
        for directory in members:
            manifest = directory / "Cargo.toml"
            package = source_package(workspace.load_manifest(manifest))
            name = package["name"]
            if name in packages:
                raise ValueError("SDK baseline has duplicate package identities")
            path = manifest.relative_to(source).as_posix()
            packages[name] = {"manifest_path": path}
            selected.append(package)
        sdk = selected_crates(selected)[1]
        if not sdk:
            raise ValueError("SDK baseline has no publishable SDK inventory")
        return {"revision": revision, "archive_sha256": hashlib.sha256(archive_bytes).hexdigest(),
                "members": packages, "sdk_crates": sdk, "source_hashes": hashes}


def observe_source(root: Path, revision: str, members: dict, sdk: list[str], *,
                   git_manifest_paths: dict[str, str] | None = None) -> dict:
    """Read only this selected source's manifests/lock through the Git owner.

    Normal/build edges include every optional/target declaration. External lock
    versions are unioned by name: absence in this conservative graph is useful;
    it is not an enabled-feature graph or an executed build observation.
    Verified historical paths stay Git-relative and never resolve through the
    current filesystem; current Cargo metadata keeps its original root check.
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
        path = (PurePosixPath(git_manifest_paths[name]).as_posix() if git_manifest_paths is not None else
                Path(members[name]["manifest_path"]).resolve().relative_to(root.resolve()).as_posix())
        document = read(path)
        package = document["package"]
        if package["name"] != name:
            raise ValueError("SDK compiler source differs from the metadata selection")
        selected.append(source_package(document))
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
            for declaration in dependency_declarations(
                document, workspace_dependencies=workspace.get("dependencies", {}),
            ):
                dependency = declaration.specification
                if "path" in dependency:
                    base = PurePosixPath() if dependency.get("workspace") else PurePosixPath(identity).parent
                    path = posixpath.normpath(str(base / dependency["path"] / "Cargo.toml"))
                    if path not in manifests:
                        manifests[path] = read(path)
                    pending.append(("manifest", path))
                else:
                    pending.append(("external", declaration.package))
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
    inventory, baseline = None, None
    if baseline_rev:
        inventory = baseline_inventory(root, baseline_rev)
        historical = set(inventory["sdk_crates"])
        if historical - set(sdk):
            raise ValueError("SDK baseline members disappeared from the current SDK selection")
        if (set(sdk) & set(inventory["members"])) - historical:
            raise ValueError("SDK package changed its historical publication identity")
        historical_paths = {name: row["manifest_path"] for name, row in inventory["members"].items()}
        baseline = observe_source(root, baseline_rev, {}, inventory["sdk_crates"],
                                  git_manifest_paths=historical_paths)
    result = {"current": current, "environment": {}, "ambient": "unused"}
    if baseline is not None:
        result["baseline"] = baseline
        result["baseline_inventory"] = inventory
        result["first_release_crates"] = sorted(set(sdk) - set(inventory["members"]))
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
