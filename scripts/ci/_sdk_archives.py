"""Archive IO for the existing SDK qualifier; no process or registry owner."""
from __future__ import annotations

import copy
import hashlib
import json
import re
import shutil
import tarfile
import tomllib
from pathlib import Path, PurePosixPath


def capture_archives(directory: Path, destination: Path, names: list[str],
                     members: dict, revision: str, root: Path) -> list[dict]:
    """Keep the package qualifier's one archive/VCS validation path."""
    destination.mkdir(exist_ok=True)
    artifacts = []
    for name in names:
        archive = directory / f"{name}-{members[name]['version']}.crate"
        data = archive.read_bytes()
        (destination / archive.name).write_bytes(data)
        normalized = {}
        try:
            package = tarfile.open(archive, "r:gz")
        except tarfile.TarError as error:
            raise ValueError(f"unreadable package archive: {name}") from error
        with package:
            for item in ("Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"):
                try:
                    reader = package.extractfile(f"{name}-{members[name]['version']}/{item}")
                except KeyError as error:
                    raise ValueError(f"missing normalized package member: {name}/{item}") from error
                if reader is None:
                    raise ValueError(f"missing normalized package member: {name}/{item}")
                contents = reader.read()
                (destination / f"{name}--{item}").write_bytes(contents)
                normalized[item] = hashlib.sha256(contents).hexdigest()
                if item == ".cargo_vcs_info.json":
                    vcs = json.loads(contents)
                    if not isinstance(vcs, dict) or not isinstance(vcs.get("git"), dict):
                        raise ValueError(f"invalid package VCS observation: {name}")
                    manifest_path = members[name].get("manifest_path")
                    if not isinstance(manifest_path, str):
                        raise ValueError(f"package metadata has no manifest location: {name}")
                    expected_path = Path(manifest_path).parent.resolve().relative_to(root.resolve()).as_posix()
                    if (vcs["git"].get("sha1") != revision
                            or vcs["git"].get("dirty", False) is not False
                            or vcs.get("path_in_vcs") != expected_path):
                        raise ValueError(f"package VCS observation does not bind the clean source: {name}")
        artifacts.append({"package": name, "path": str(destination / archive.name),
                          "sha256": hashlib.sha256(data).hexdigest(), "normalized": normalized})
    return artifacts


def consumer_packages(manifest: Path, members: dict, sdk: list[str]) -> list[str]:
    """SDK selection stays unchanged; include the real consumer's other peers."""
    authored = tomllib.loads(manifest.read_text())
    pending = list(sdk)
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        for alias, dependency in authored.get(section, {}).items():
            if isinstance(dependency, dict) and "path" in dependency:
                pending.append(dependency.get("package", alias))
    seen = set()
    while pending:
        name = pending.pop()
        if name in seen:
            continue
        if name not in members:
            raise ValueError(f"consumer path dependency is not a source workspace member: {name}")
        seen.add(name)
        for dependency in members[name]["dependencies"]:
            if dependency.get("kind") != "dev" and dependency.get("path"):
                pending.append(dependency["name"])
    return sorted(seen)


def file_hashes(directory: Path) -> dict[str, str]:
    return {p.relative_to(directory).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(directory.rglob("*")) if p.is_file()}


def unpack_archives(artifacts: list[dict], members: dict, destination: Path) -> dict[str, Path]:
    """Only real archive files enter this private extraction directory."""
    destination.mkdir()
    unpacked = {}
    for artifact in artifacts:
        name = artifact["package"]
        archive = Path(artifact["path"])
        if hashlib.sha256(archive.read_bytes()).hexdigest() != artifact["sha256"]:
            raise ValueError(f"package archive changed before extraction: {name}")
        prefix = f"{name}-{members[name]['version']}"
        seen = set()
        with tarfile.open(archive, "r:gz") as package:
            for item in package:
                path = PurePosixPath(item.name)
                if (path.is_absolute() or ".." in path.parts or not path.parts
                        or path.parts[0] != prefix or item.name in seen
                        or not (item.isfile() or item.isdir())):
                    raise ValueError(f"invalid package archive member: {name}/{item.name}")
                seen.add(item.name)
                target = destination.joinpath(*path.parts)
                if item.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    reader = package.extractfile(item)
                    if reader is None:
                        raise ValueError(f"unreadable package archive member: {name}/{item.name}")
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(reader.read())
                    target.chmod(item.mode & 0o777)
        unpacked[name] = destination / prefix
        if not (unpacked[name] / "Cargo.toml").is_file():
            raise ValueError(f"package archive has no manifest: {name}")
    return unpacked


def stage_consumer(manifest: Path, destination: Path, unpacked: dict[str, Path], members: dict) -> Path:
    """Copy original tasks, changing dependency locations rather than behavior."""
    destination.mkdir()
    for directory in ("src", "tests"):
        shutil.copytree(manifest.parent / directory, destination / directory)
    text = manifest.read_text()
    expected = copy.deepcopy(tomllib.loads(text))
    if "patch" in expected or "target" in expected:
        raise ValueError("artifact consumer requires the reviewed original dependency tables")
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        for alias, dependency in expected.get(section, {}).items():
            if not isinstance(dependency, dict) or "path" not in dependency:
                continue
            name = dependency.get("package", alias)
            if name not in unpacked:
                raise ValueError(f"consumer has no package archive: {name}")
            authored_version = "version" in dependency
            dependency.pop("path")
            dependency["version"] = "=" + members[name]["version"]
            pattern = re.compile(r"(?m)^(" + re.escape(alias) + r"\s*=\s*)\{([^\n]*)\}(\s*)$")
            def replace(match):
                path_field = r'\bpath\s*=\s*"[^"\n]*"'
                version_field = "version = " + json.dumps(dependency["version"])
                if authored_version:
                    body, count = re.subn(r'\bversion\s*=\s*"[^"\n]*"', version_field, match[2])
                    if count != 1:
                        raise ValueError(f"consumer version is not the reviewed inline value: {alias}")
                    body, count = re.subn(path_field + r'\s*,\s*', "", body)
                    if count == 0:
                        body, count = re.subn(r',\s*' + path_field, "", body)
                else:
                    body, count = re.subn(path_field, version_field, match[2])
                if count != 1:
                    raise ValueError(f"consumer dependency is not the reviewed inline path: {alias}")
                return match[1] + "{" + body + "}" + match[3]
            text, count = pattern.subn(replace, text)
            if count != 1:
                raise ValueError(f"consumer dependency is not unique: {alias}")
    expected["patch"] = {"crates-io": {name: {"path": str(path)} for name, path in unpacked.items()}}
    text += "\n[patch.crates-io]\n" + "".join(
        name + " = { path = " + json.dumps(str(path)) + " }\n" for name, path in unpacked.items())
    if tomllib.loads(text) != expected:
        raise ValueError("artifact consumer changed something other than its dependency locations")
    target = destination / "Cargo.toml"
    target.write_text(text)
    return target


def validate_consumer_graph(metadata: dict, consumer: Path, unpacked: dict[str, Path],
                            members: dict) -> list[str]:
    """No source-checkout or registry fallback may masquerade as archive IO."""
    if Path(metadata["workspace_root"]).resolve() != consumer.resolve() or len(metadata["workspace_members"]) != 1:
        raise ValueError("artifact consumer must be its own one-member workspace")
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    if len(packages) != len(metadata["packages"]) or len(nodes) != len(metadata["resolve"]["nodes"]):
        raise ValueError("artifact consumer graph contains duplicate identities")
    pending, seen, consumed = list(metadata["workspace_members"]), set(), set()
    while pending:
        identity = pending.pop()
        if identity in seen:
            continue
        seen.add(identity)
        package, node = packages[identity], nodes[identity]
        name = package["name"]
        manifest = Path(package["manifest_path"]).resolve()
        if name in members:
            if (name not in unpacked or package["version"] != members[name]["version"]
                    or manifest != (unpacked[name] / "Cargo.toml").resolve()
                    or package.get("source") is not None):
                raise ValueError(f"consumer dependency did not come from its archive: {name}")
            consumed.add(name)
        elif package.get("source") is None and manifest != (consumer / "Cargo.toml").resolve():
            raise ValueError(f"consumer has an unarchived path dependency: {name}")
        pending.extend(dependency["pkg"] for dependency in node["deps"])
    authored = tomllib.loads((consumer / "Cargo.toml").read_text())
    required = {dependency.get("package", alias)
                for section in ("dependencies", "dev-dependencies", "build-dependencies")
                for alias, dependency in authored.get(section, {}).items()
                if isinstance(dependency, dict) and dependency.get("package", alias) in members}
    if not consumed or not required.issubset(consumed):
        raise ValueError("artifact consumer graph omits its declared archived dependencies")
    return sorted(consumed)
