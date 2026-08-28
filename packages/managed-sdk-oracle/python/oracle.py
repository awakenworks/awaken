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
    source_hashes = []
    for resource_root, filename, prefix in sorted(files, key=lambda item: str(item[1])):
        relative = filename.relative_to(root).as_posix()
        source_hashes.append({"path": relative, "sha256": hashlib.sha256(filename.read_bytes()).hexdigest()})
        for operation in operations_in_file(resource_root, filename, prefix):
            previous = by_id.get(operation["id"])
            if previous is not None and previous != operation:
                raise AssertionError(f"{version}: conflicting operation {operation['id']}")
            by_id[operation["id"]] = operation
    operations = sorted(by_id.values(), key=lambda operation: operation["id"])
    if not operations:
        raise AssertionError(f"{version}: no Managed operations extracted")
    return {
        "version": version,
        "operation_fingerprint": digest(operations),
        "source_fingerprint": digest(source_hashes),
        "source_file_count": len(source_hashes),
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
                "source_fingerprint", "source_file_count",
            )
        } | {
            "operation_count": len(anchor["operations"]),
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
