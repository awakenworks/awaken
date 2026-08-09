#!/usr/bin/env python3
"""Require reproducible, allowlisted sources for every Cargo Git dependency."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
FULL_GIT_REV = re.compile(r"[0-9a-f]{40}")
ALLOWED_GIT_REPOSITORIES = {
    "https://github.com/awakenworks/awaken-foundation",
    "https://github.com/awakenworks/awaken-iam",
}
DEPENDENCY_TABLES = {"dependencies", "dev-dependencies", "build-dependencies"}


def normalized_repository(raw_url: str) -> tuple[str | None, str | None]:
    parsed = urlsplit(raw_url)
    if parsed.scheme != "https":
        return None, "Git dependency URL must use HTTPS"
    if parsed.username is not None or parsed.password is not None:
        return None, "Git dependency URL must not contain credentials"
    if parsed.query or parsed.fragment:
        return None, "manifest Git dependency URL must not contain a query or fragment"
    path = parsed.path.rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    repository = f"https://{parsed.netloc.lower()}{path}"
    if repository not in ALLOWED_GIT_REPOSITORIES:
        return None, f"Git dependency repository is not allowlisted: {repository}"
    return repository, None


def dependency_specs(value: object, path: str = "") -> list[tuple[str, dict[str, object]]]:
    found: list[tuple[str, dict[str, object]]] = []
    if not isinstance(value, dict):
        return found
    for key, child in value.items():
        child_path = f"{path}.{key}" if path else key
        if key in DEPENDENCY_TABLES and isinstance(child, dict):
            for name, spec in child.items():
                if isinstance(spec, dict) and "git" in spec:
                    found.append((f"{child_path}.{name}", spec))
        else:
            found.extend(dependency_specs(child, child_path))
    return found


def validate_manifest_data(data: dict[str, object], label: str) -> list[str]:
    violations: list[str] = []
    for path, spec in dependency_specs(data):
        git_url = spec.get("git")
        if not isinstance(git_url, str):
            violations.append(f"{label}:{path}: git must be a URL string")
            continue
        _, error = normalized_repository(git_url)
        if error:
            violations.append(f"{label}:{path}: {error}")
        revision = spec.get("rev")
        if not isinstance(revision, str) or FULL_GIT_REV.fullmatch(revision) is None:
            violations.append(f"{label}:{path}: Git dependency must pin a full 40-character rev")
        if "branch" in spec or "tag" in spec:
            violations.append(f"{label}:{path}: branch/tag pins are not reproducible dependency inputs")
    return violations


def validate_lock_data(data: dict[str, object], label: str) -> list[str]:
    violations: list[str] = []
    packages = data.get("package", [])
    if not isinstance(packages, list):
        return [f"{label}: package must be an array"]
    for package in packages:
        if not isinstance(package, dict):
            continue
        source = package.get("source")
        if not isinstance(source, str) or not source.startswith("git+"):
            continue
        name = package.get("name", "<unknown>")
        parsed = urlsplit(source.removeprefix("git+"))
        repository_url = f"{parsed.scheme}://{parsed.netloc}{parsed.path}"
        _, error = normalized_repository(repository_url)
        if error:
            violations.append(f"{label}:{name}: {error}")
        revisions = parse_qs(parsed.query).get("rev", [])
        if len(revisions) != 1 or FULL_GIT_REV.fullmatch(revisions[0]) is None:
            violations.append(f"{label}:{name}: locked Git source must retain one full rev")
            continue
        if parsed.fragment != revisions[0]:
            violations.append(f"{label}:{name}: locked Git commit does not match requested rev")
    return violations


def tracked_manifests() -> list[Path]:
    try:
        output = subprocess.check_output(
            ["git", "ls-files"], cwd=REPO_ROOT, text=True, stderr=subprocess.DEVNULL
        )
    except (OSError, subprocess.CalledProcessError):
        return sorted(REPO_ROOT.rglob("Cargo.toml"))
    return [REPO_ROOT / line for line in output.splitlines() if line.endswith("Cargo.toml")]


def load_toml(path: Path) -> dict[str, object]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def check_repository() -> list[str]:
    violations: list[str] = []
    for manifest in tracked_manifests():
        relative = manifest.relative_to(REPO_ROOT).as_posix()
        violations.extend(validate_manifest_data(load_toml(manifest), relative))
    lockfile = REPO_ROOT / "Cargo.lock"
    if lockfile.is_file():
        violations.extend(validate_lock_data(load_toml(lockfile), "Cargo.lock"))
    return violations


def self_test() -> int:
    revision = "1" * 40
    allowed = "https://github.com/awakenworks/awaken-foundation"
    good_manifest = {"workspace": {"dependencies": {"x": {"git": allowed, "rev": revision}}}}
    if validate_manifest_data(good_manifest, "good"):
        print("Self-test FAILED: valid manifest was rejected", file=sys.stderr)
        return 1

    bad_manifest = {
        "target": {
            "cfg(unix)": {
                "dependencies": {
                    "private": {"git": "ssh://user@192.168.1.2/repo", "rev": revision},
                    "foreign": {"git": "https://example.com/repo", "rev": revision},
                    "floating": {"git": allowed, "branch": "main"},
                }
            }
        }
    }
    manifest_hits = validate_manifest_data(bad_manifest, "bad")
    expected = {
        "must use HTTPS",
        "not allowlisted",
        "full 40-character rev",
        "branch/tag pins",
    }
    if any(not any(fragment in hit for hit in manifest_hits) for fragment in expected):
        print("Self-test FAILED: manifest policy causes were not all detected", file=sys.stderr)
        return 1

    good_lock = {
        "package": [
            {
                "name": "x",
                "source": f"git+{allowed}?rev={revision}#{revision}",
            }
        ]
    }
    if validate_lock_data(good_lock, "good-lock"):
        print("Self-test FAILED: valid lock source was rejected", file=sys.stderr)
        return 1
    bad_lock = {
        "package": [
            {
                "name": "x",
                "source": f"git+{allowed}?rev={revision}#{'2' * 40}",
            }
        ]
    }
    if not any("does not match" in hit for hit in validate_lock_data(bad_lock, "bad-lock")):
        print("Self-test FAILED: lock revision drift was not detected", file=sys.stderr)
        return 1
    print("OK - dependency-source self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    try:
        violations = check_repository()
    except (OSError, tomllib.TOMLDecodeError) as error:
        print(f"ERROR: dependency-source check could not read Cargo metadata: {error}", file=sys.stderr)
        return 1
    if violations:
        print("Dependency-source check FAILED:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print("OK - Cargo Git dependencies use allowlisted HTTPS repositories and full revisions.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
