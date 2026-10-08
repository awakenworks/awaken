#!/usr/bin/env python3
"""Union homogeneous LCOV groups into per-workspace-crate production coverage."""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from check_changed_e2e_line_coverage import test_only_lines

ROOT = Path(__file__).resolve().parents[2]


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
    result = []
    for name, directory in sorted(packages):
        def counts(values: dict) -> dict | None:
            # Only production source is counted; integration tests/build scripts
            # and dependencies cannot inflate a crate's production percentage.
            owned = [hits for key, hits in values.items() if key[0].startswith(directory + "/src/")]
            return {"covered": sum(hits > 0 for hits in owned), "total": len(owned)} if owned else None
        result.append({"crate": name, "path": directory, "lines": counts(lines), "branches": counts(branches)})
    return result


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--lcov-path", type=Path, action="append", required=True)
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
    excluded = {p: test_only_lines(p) for p in files}
    report = {"revision": args.revision, "source_digest": args.source_digest, "scope": "workspace-production", "crates": summarize(metadata, documents, ROOT, excluded), "unmeasured": "null means unavailable or not linked; it never means zero coverage"}
    args.output.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"Per-crate coverage: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
