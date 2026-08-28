#!/usr/bin/env python3
"""Generate and verify the official Python Managed Agents SDK oracle."""

from __future__ import annotations

import argparse
import ast
import datetime
import hashlib
import json
import re
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


PACKAGE_ROOT = Path(__file__).resolve().parent.parent
REPO_ROOT = PACKAGE_ROOT.parent.parent
ANCHORS_PATH = PACKAGE_ROOT / "config/python-anchors.json"
SCOPE_PATH = PACKAGE_ROOT / "config/scope.json"
ORACLE_PATH = REPO_ROOT / "contracts/anthropic-managed/python-upstream-oracle.generated.json"
BETA_RE = re.compile(r"^[a-z][a-z0-9-]*-\d{4}-\d{2}-\d{2}$")
PLACEHOLDER_RE = re.compile(r"\{[^{}]+\}")
HTTP_CALLS = {
    "_delete": "DELETE",
    "_get": "GET",
    "_get_api_list": "GET",
    "_patch": "PATCH",
    "_post": "POST",
    "_put": "PUT",
}


@dataclass(frozen=True)
class Wheel:
    version: str
    url: str
    sha256: str
    filename: str


def stable(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: stable(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [stable(item) for item in value]
    return value


def digest(value: Any) -> str:
    encoded = json.dumps(stable(value), separators=(",", ":"), ensure_ascii=False).encode()
    return hashlib.sha256(encoded).hexdigest()


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def fetch_json(url: str) -> Any:
    with urllib.request.urlopen(url, timeout=60) as response:  # noqa: S310 - fixed PyPI origin
        return json.load(response)


def resolve_wheel(package: str, version: str) -> Wheel:
    release = fetch_json(f"https://pypi.org/pypi/{package}/{version}/json")
    wheels = [
        item
        for item in release["urls"]
        if item["packagetype"] == "bdist_wheel" and item["filename"].endswith("-py3-none-any.whl")
    ]
    if len(wheels) != 1:
        raise AssertionError(f"{package} {version}: expected one universal wheel, got {len(wheels)}")
    item = wheels[0]
    return Wheel(version, item["url"], item["digests"]["sha256"], item["filename"])


def download_wheel(wheel: Wheel, directory: Path) -> Path:
    target = directory / wheel.filename
    with urllib.request.urlopen(wheel.url, timeout=120) as response:  # noqa: S310 - PyPI URL from signed metadata
        body = response.read()
    actual = hashlib.sha256(body).hexdigest()
    if actual != wheel.sha256:
        raise AssertionError(f"{wheel.filename}: sha256 {actual} != PyPI {wheel.sha256}")
    target.write_bytes(body)
    return target


def extract_wheel(wheel_path: Path, destination: Path) -> None:
    root = destination.resolve()
    with zipfile.ZipFile(wheel_path) as archive:
        for member in archive.infolist():
            target = (destination / member.filename).resolve()
            if target != root and root not in target.parents:
                raise AssertionError(f"{wheel_path.name}: unsafe wheel member {member.filename!r}")
        archive.extractall(destination)


def expression_text(node: ast.AST) -> str:
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.Call) and node.args:
        return expression_text(node.args[0])
    if isinstance(node, ast.JoinedStr):
        result = []
        for value in node.values:
            if isinstance(value, ast.Constant):
                result.append(str(value.value))
            else:
                result.append("{parameter}")
        return "".join(result)
    raise AssertionError(f"unsupported Python SDK route expression: {ast.dump(node, include_attributes=False)}")


def normalized_route(route: str) -> tuple[str, str | None]:
    path, separator, query = route.partition("?")
    path = PLACEHOLDER_RE.sub("{}", path)
    return path, query if separator else None


def namespace(resource_root: Path, filename: Path, prefix: str) -> str:
    relative = filename.relative_to(resource_root).with_suffix("")
    parts = list(relative.parts)
    if parts[-1] == "__init__":
        return ""
    if len(parts) > 1 and parts[-1] == parts[-2]:
        parts.pop()
    return ".".join(([prefix] if prefix else []) + parts)


def is_sync_resource(node: ast.ClassDef) -> bool:
    return any(isinstance(base, ast.Name) and base.id == "SyncAPIResource" for base in node.bases)


def is_async_resource(node: ast.ClassDef) -> bool:
    return any(isinstance(base, ast.Name) and base.id == "AsyncAPIResource" for base in node.bases)


def decorator_name(node: ast.AST) -> str | None:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return node.attr
    if isinstance(node, ast.Call):
        return decorator_name(node.func)
    return None


def helper_methods_in_file(resource_root: Path, filename: Path, prefix: str) -> list[str]:
    """Extract public async-resource helpers that do not issue one HTTP call.

    Generated sub-resource accessors are cached properties; ordinary generated
    operations contain one transport call. What remains is the small explicit
    helper surface (currently poller, worker, and Session tool_runner). Keeping
    this structural rule in the oracle makes a newly added helper fail closed
    instead of relying on a reviewer to notice one more handwritten SDK API.
    """
    module = ast.parse(filename.read_text(encoding="utf-8"), filename=str(filename))
    helper_namespace = namespace(resource_root, filename, prefix)
    if not helper_namespace:
        return []
    helpers = set()
    for resource in (node for node in module.body if isinstance(node, ast.ClassDef) and is_async_resource(node)):
        for method in (
            node
            for node in resource.body
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
        ):
            if method.name.startswith("_"):
                continue
            decorators = {name for item in method.decorator_list if (name := decorator_name(item))}
            if decorators & {"cached_property", "property"}:
                continue
            transport_calls = [
                candidate
                for candidate in ast.walk(method)
                if isinstance(candidate, ast.Call)
                and isinstance(candidate.func, ast.Attribute)
                and candidate.func.attr in HTTP_CALLS
            ]
            if not transport_calls:
                helpers.add(f"{helper_namespace}.{method.name}")
    return sorted(helpers)


def public_exports_in_file(filename: Path, module_name: str) -> list[str]:
    """Return the exact explicit public surface of one handwritten module."""
    module = ast.parse(filename.read_text(encoding="utf-8"), filename=str(filename))
    assignments = [
        node
        for node in module.body
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "__all__" for target in node.targets)
    ]
    if len(assignments) != 1:
        raise AssertionError(f"{module_name}: expected one explicit __all__, got {len(assignments)}")
    value = assignments[0].value
    if not isinstance(value, (ast.List, ast.Tuple)):
        raise AssertionError(f"{module_name}: __all__ must be a literal list or tuple")
    names = []
    for item in value.elts:
        if not isinstance(item, ast.Constant) or not isinstance(item.value, str):
            raise AssertionError(f"{module_name}: __all__ contains a dynamic export")
        names.append(item.value)
    if len(names) != len(set(names)):
        raise AssertionError(f"{module_name}: __all__ contains duplicate exports")
    return sorted(f"{module_name}.{name}" for name in names)


def managed_library_exports(root: Path, module_names: Iterable[str]) -> tuple[list[str], list[dict[str, str]]]:
    exports = []
    source_hashes = []
    for module_name in module_names:
        module_path = root.joinpath(*module_name.split("."))
        filename = module_path / "__init__.py" if module_path.is_dir() else module_path.with_suffix(".py")
        if not filename.is_file():
            continue
        exports.extend(public_exports_in_file(filename, module_name))
        source_hashes.append({
            "path": filename.relative_to(root).as_posix(),
            "sha256": hashlib.sha256(filename.read_bytes()).hexdigest(),
        })
    if len(exports) != len(set(exports)):
        raise AssertionError("Python Managed library modules expose duplicate identities")
    return sorted(exports), source_hashes


def beta_tokens(node: ast.AST) -> list[str]:
    return sorted({
        child.value
        for child in ast.walk(node)
        if isinstance(child, ast.Constant)
        and isinstance(child.value, str)
        and BETA_RE.fullmatch(child.value)
    })


def operations_in_file(resource_root: Path, filename: Path, prefix: str) -> list[dict[str, Any]]:
    module = ast.parse(filename.read_text(encoding="utf-8"), filename=str(filename))
    operation_namespace = namespace(resource_root, filename, prefix)
    if not operation_namespace:
        return []
    operations = []
    for resource in (node for node in module.body if isinstance(node, ast.ClassDef) and is_sync_resource(node)):
        for method in (node for node in resource.body if isinstance(node, ast.FunctionDef)):
            calls = []
            for candidate in ast.walk(method):
                if not isinstance(candidate, ast.Call) or not isinstance(candidate.func, ast.Attribute):
                    continue
                verb = HTTP_CALLS.get(candidate.func.attr)
                if verb and candidate.args:
                    calls.append((verb, candidate))
            if len(calls) > 1:
                raise AssertionError(f"{filename}:{method.name} contains multiple HTTP operations")
            if not calls:
                continue
            verb, call = calls[0]
            route, query = normalized_route(expression_text(call.args[0]))
            operation = {
                "id": f"{operation_namespace}.{method.name}",
                "method": verb,
                "path": route,
                "betas": beta_tokens(method),
            }
            if query:
                operation["transport_query"] = query
            operations.append(operation)
    return operations


def scoped_files(resources: Path, roots: Iterable[str], prefix: str) -> list[tuple[Path, Path, str]]:
    selected = []
    for root in roots:
        python_root = root.replace("-", "_")
        candidate = resources / python_root
        if candidate.is_file():
            selected.append((resources, candidate, prefix))
        elif candidate.with_suffix(".py").is_file():
            selected.append((resources, candidate.with_suffix(".py"), prefix))
        elif candidate.is_dir():
            selected.extend((resources, path, prefix) for path in candidate.rglob("*.py"))
    return selected


def extract(root: Path, version: str, scope: dict[str, Any]) -> dict[str, Any]:
    resources = root / "anthropic/resources"
    files = [
        *scoped_files(resources / "beta", scope["beta_resource_roots"], "beta"),
        *scoped_files(resources, scope["ga_resource_roots"], ""),
    ]
    by_id: dict[str, dict[str, Any]] = {}
    helpers: set[str] = set()
    source_hashes = []
    for resource_root, filename, prefix in sorted(files, key=lambda item: str(item[1])):
        relative = filename.relative_to(root).as_posix()
        source_hashes.append({"path": relative, "sha256": hashlib.sha256(filename.read_bytes()).hexdigest()})
        helpers.update(helper_methods_in_file(resource_root, filename, prefix))
        for operation in operations_in_file(resource_root, filename, prefix):
            previous = by_id.get(operation["id"])
            if previous is not None and previous != operation:
                raise AssertionError(f"{version}: conflicting operation {operation['id']}")
            by_id[operation["id"]] = operation
    library_exports, library_source_hashes = managed_library_exports(
        root,
        scope["python_managed_library_modules"],
    )
    source_hashes.extend(library_source_hashes)
    source_hashes.sort(key=lambda item: item["path"])
    operations = sorted(by_id.values(), key=lambda operation: operation["id"])
    if not operations:
        raise AssertionError(f"{version}: no Managed operations extracted")
    return {
        "version": version,
        "operation_fingerprint": digest(operations),
        "source_fingerprint": digest(source_hashes),
        "source_file_count": len(source_hashes),
        "helper_fingerprint": digest(sorted(helpers)),
        "helpers": sorted(helpers),
        "library_export_fingerprint": digest(library_exports),
        "library_exports": library_exports,
        "operations": operations,
    }


def generate() -> dict[str, Any]:
    anchors = load_json(ANCHORS_PATH)
    extracted = [extract_anchor(anchors["package"], configured) for configured in anchors["anchors"]]
    current = [anchor for anchor in extracted if anchor["role"] == "current_oracle"]
    if len(current) != 1:
        raise AssertionError("exactly one Python current oracle is required")
    current_ids = {operation["id"] for operation in current[0]["operations"]}
    summaries = []
    for anchor in extracted:
        ids = {operation["id"] for operation in anchor["operations"]}
        summaries.append({
            key: anchor[key]
            for key in (
                "version", "role", "reason", "wheel", "operation_fingerprint",
                "source_fingerprint", "source_file_count", "helper_fingerprint",
                "library_export_fingerprint",
            )
        } | {
            "operation_count": len(anchor["operations"]),
            "helper_count": len(anchor["helpers"]),
            "helpers": anchor["helpers"],
            "library_export_count": len(anchor["library_exports"]),
            "library_exports": anchor["library_exports"],
            "only_in_anchor": sorted(ids - current_ids),
            "only_in_current": sorted(current_ids - ids),
        })
    return stable({
        "schema_version": 1,
        "package": anchors["package"],
        "current": current[0],
        "anchors": summaries,
    })


def extract_anchor(package: str, configured: dict[str, str]) -> dict[str, Any]:
    scope = load_json(SCOPE_PATH)
    wheel = resolve_wheel(package, configured["version"])
    with tempfile.TemporaryDirectory(prefix="awaken-python-sdk-anchor-") as temporary:
        temporary_path = Path(temporary)
        wheel_path = download_wheel(wheel, temporary_path)
        destination = temporary_path / configured["version"]
        extract_wheel(wheel_path, destination)
        evidence = extract(destination, configured["version"], scope)
    return stable({
        "role": configured["role"],
        "reason": configured["reason"],
        "wheel": {"filename": wheel.filename, "sha256": wheel.sha256},
        **evidence,
    })


def minimum_release_age_minutes() -> int:
    workspace = (REPO_ROOT / "pnpm-workspace.yaml").read_text(encoding="utf-8")
    match = re.search(r"^minimumReleaseAge:\s*(\d+)\s*$", workspace, re.MULTILINE)
    if not match or int(match.group(1)) <= 0:
        raise AssertionError("Python canary requires one positive minimumReleaseAge policy")
    return int(match.group(1))


def canary(expected: dict[str, Any]) -> str:
    package = expected["package"]
    release = fetch_json(f"https://pypi.org/pypi/{package}/json")
    latest = release["info"]["version"]
    current = expected["current"]["version"]
    status = f"registry={latest}"
    if latest != current:
        uploads = release["releases"].get(latest, [])
        timestamps = [item.get("upload_time_iso_8601") for item in uploads]
        timestamps = [value for value in timestamps if value]
        if not timestamps:
            raise AssertionError(f"PyPI latest {latest} has no publication timestamp")
        published = min(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")) for value in timestamps)
        eligible = published + datetime.timedelta(minutes=minimum_release_age_minutes())
        now = datetime.datetime.now(datetime.timezone.utc)
        if now >= eligible:
            raise AssertionError(
                f"PyPI latest anthropic {latest} does not match Python current oracle {current}; "
                "review its change points and regenerate"
            )
        status += f" quarantined_until={eligible.isoformat().replace('+00:00', 'Z')}"
    configured = load_json(ANCHORS_PATH)
    current_config = [anchor for anchor in configured["anchors"] if anchor["role"] == "current_oracle"]
    if len(current_config) != 1:
        raise AssertionError("exactly one configured Python current oracle is required")
    actual = extract_anchor(package, current_config[0])
    if actual != expected["current"]:
        raise AssertionError("official Python current wheel drifted from its reviewed oracle")
    return status


def validate(oracle: dict[str, Any]) -> None:
    if oracle.get("schema_version") != 1 or oracle.get("package") != "anthropic":
        raise AssertionError("unsupported Python Managed SDK oracle")
    operations = oracle["current"]["operations"]
    if digest(operations) != oracle["current"]["operation_fingerprint"]:
        raise AssertionError("current Python operation fingerprint is stale")
    helpers = oracle["current"].get("helpers")
    if not isinstance(helpers, list) or helpers != sorted(set(helpers)):
        raise AssertionError("current Python helpers must be unique and sorted")
    if digest(helpers) != oracle["current"].get("helper_fingerprint"):
        raise AssertionError("current Python helper fingerprint is stale")
    library_exports = oracle["current"].get("library_exports")
    if not isinstance(library_exports, list) or library_exports != sorted(set(library_exports)):
        raise AssertionError("current Python library exports must be unique and sorted")
    if digest(library_exports) != oracle["current"].get("library_export_fingerprint"):
        raise AssertionError("current Python library export fingerprint is stale")
    ids = [operation["id"] for operation in operations]
    if ids != sorted(set(ids)):
        raise AssertionError("current Python operations must be unique and sorted")
    configured = load_json(ANCHORS_PATH)["anchors"]
    expected = [(anchor["version"], anchor["role"], anchor["reason"]) for anchor in configured]
    actual = [(anchor["version"], anchor["role"], anchor["reason"]) for anchor in oracle["anchors"]]
    if actual != expected:
        raise AssertionError("Python anchor summaries differ from the reviewed change-point policy")
    if len({anchor["wheel"]["sha256"] for anchor in oracle["anchors"]}) != len(actual):
        raise AssertionError("each Python anchor must bind one distinct official wheel")
    for anchor in oracle["anchors"]:
        helpers = anchor.get("helpers")
        if not isinstance(helpers, list) or helpers != sorted(set(helpers)):
            raise AssertionError(f"{anchor['version']}: helpers must be unique and sorted")
        if anchor.get("helper_count") != len(helpers) or anchor.get("helper_fingerprint") != digest(helpers):
            raise AssertionError(f"{anchor['version']}: helper summary is stale")
        exports = anchor.get("library_exports")
        if not isinstance(exports, list) or exports != sorted(set(exports)):
            raise AssertionError(f"{anchor['version']}: library exports must be unique and sorted")
        if (
            anchor.get("library_export_count") != len(exports)
            or anchor.get("library_export_fingerprint") != digest(exports)
        ):
            raise AssertionError(f"{anchor['version']}: library export summary is stale")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("generate", "check", "canary"))
    args = parser.parse_args()
    if args.command == "generate":
        result = generate()
        ORACLE_PATH.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(f"Generated Python Managed SDK oracle for {result['current']['version']}")
        return
    expected = load_json(ORACLE_PATH)
    validate(expected)
    if args.command == "canary":
        registry_status = canary(expected)
        print(
            f"Python Managed SDK oracle is current at {expected['current']['version']}; "
            f"{registry_status}"
        )
    else:
        print(f"Python Managed SDK oracle is current at {expected['current']['version']}")


if __name__ == "__main__":
    main()
