#!/usr/bin/env python3
"""Union homogeneous LCOV groups into per-workspace-crate production coverage."""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import _crate_boundary_workspace as workspace
from check_changed_e2e_line_coverage import test_only_lines

ROOT = Path(__file__).resolve().parents[2]


def production_exclusions(metadata: dict, files: set[str], root: Path) -> tuple[dict[str, set[int]], dict[str, list[str]]]:
    """Reuse Cargo module ownership; absence outside a known graph proves nothing."""
    members = set(metadata["workspace_members"])
    production: set[Path] = set()
    compiled: set[Path] = set()
    unresolved: dict[str, list[str]] = {}
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        manifest = Path(package["manifest_path"])
        unknown: list[str] = []
        production.update(workspace.production_module_files([manifest], unresolved_includes=unknown))
        if unknown:
            # Never turn an unresolved include into a negative usage verdict.
            # The caller leaves this crate's production coverage unmeasured.
            unresolved[package["name"]] = unknown
        else:
            compiled.update(workspace.production_module_files(
                [manifest], project=workspace.rust_without_comments, include_test_targets=True))
    test_files = compiled - production
    excluded = {}
    for relative in files:
        path = root / relative
        excluded[relative] = (set(range(1, len(path.read_text(encoding="utf-8").splitlines()) + 1))
                              if path.resolve() in test_files else test_only_lines(relative, root=root))
    return excluded, unresolved


def summarize(metadata: dict, documents: list[str], root: Path, excluded: dict[str, set[int]]) -> list[dict]:
    members = set(metadata["workspace_members"])
    packages = [(p["name"], Path(p["manifest_path"]).parent.relative_to(root).as_posix()) for p in metadata["packages"] if p["id"] in members]
    if not packages:
        raise ValueError("workspace coverage corpus is empty")
    lines: dict[tuple, int] = {}
    branches: dict[tuple, int] = {}
    for document in documents:
        current = None
        for entry in document.splitlines():
            if entry.startswith("SF:"):
                path = Path(entry[3:])
                try:
                    current = path.relative_to(root).as_posix() if path.is_absolute() else path.as_posix()
                except ValueError:
                    current = None
            elif entry == "end_of_record":
                current = None
            elif current is not None and entry.startswith("DA:"):
                number, hits, *_ = entry[3:].split(",")
                key = (current, int(number))
                if key[1] not in excluded.get(current, set()):
                    lines[key] = max(lines.get(key, 0), int(hits))
            elif current is not None and entry.startswith("BRDA:"):
                number, block, branch, hits = entry[5:].split(",")
                key = (current, int(number), block, branch)
                if key[1] not in excluded.get(current, set()):
                    branches[key] = max(branches.get(key, 0), 0 if hits == "-" else int(hits))
    # Match ownership once per source file, then aggregate each measured key
    # once. Scanning all line/branch records separately for every crate made
    # report cost proportional to crates * measured records.
    packages = sorted(packages)
    result = [{"crate": name, "path": directory, "lines": None, "branches": None}
              for name, directory in packages]
    owners: dict[str, tuple[int, ...]] = {}
    for kind, values in (("lines", lines), ("branches", branches)):
        for key, hits in values.items():
            path = key[0]
            if path not in owners:
                owners[path] = tuple(index for index, (_, directory) in enumerate(packages)
                                     if path.startswith(directory + "/src/"))
            for index in owners[path]:
                counts = result[index][kind]
                if counts is None:
                    counts = result[index][kind] = {"covered": 0, "total": 0}
                counts["covered"] += int(hits > 0)
                counts["total"] += 1
    return result


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--lcov-path", type=Path, action="append", required=True)
    parser.add_argument("--execution-scope", choices=("combined", "rust-workspace"), required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--source-digest", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    documents = [p.read_text(encoding="utf-8") for p in args.lcov_path]
    if not any("DA:" in d for d in documents):
        parser.error("LCOV reports contain no measured lines")
    files = set()
    for document in documents:
        for line in document.splitlines():
            if line.startswith("SF:"):
                path = Path(line[3:])
                try:
                    relative = path.relative_to(ROOT) if path.is_absolute() else path
                    if (ROOT / relative).is_file():
                        files.add(relative.as_posix())
                except ValueError:
                    continue
    excluded, unresolved = production_exclusions(metadata, files, ROOT)
    crates = summarize(metadata, documents, ROOT, excluded)
    for crate in crates:
        if crate["crate"] in unresolved:
            crate.update(lines=None, branches=None, unmeasured_reason="unresolved production module include")
    report = {"revision": args.revision, "source_digest": args.source_digest, "scope": "workspace-production", "execution_scope": args.execution_scope, "crates": crates, "unresolved_production_includes": unresolved, "unmeasured": "null means unavailable, not linked, or production attribution is unresolved; it never means zero coverage"}
    args.output.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"Per-crate coverage: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
