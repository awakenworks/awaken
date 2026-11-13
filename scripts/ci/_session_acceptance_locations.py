"""Resolve immutable coverage locations through unchanged, unique source blocks.

The committed binding file supplies its original source revision. Line numbers
remain historical anchors; a shift may move their exact bytes, but a changed or
ambiguous anchor cannot inherit the old reference. This changes no execution
witness, evidence digest, coverage relation or acceptance state.
"""
from __future__ import annotations

import difflib
import json
import re
import subprocess
from pathlib import Path

import formal_sources


def _git(root: Path, *arguments: str) -> str | None:
    result = subprocess.run(["git", "-C", str(root), *arguments],
                            capture_output=True, text=True, check=False,
                            env=formal_sources.detached_git_environment())
    return result.stdout if result.returncode == 0 else None


def _bindings(root: Path, ledger: Path, cache: dict) -> dict:
    key = ("committed-location-bindings",)
    if key in cache:
        return cache[key]
    result = {}
    catalog = root / ledger / "catalog.json"
    if catalog.is_file():
        for part in json.loads(catalog.read_text(encoding="utf-8"))["parts"]:
            if part["key"] != "bindings":
                continue
            relative = (ledger / part["file"]).as_posix()
            revision = _git(root, "log", "-1", "--format=%H", "HEAD", "--", relative)
            if not revision or not revision.strip():
                continue
            revision = revision.strip()
            raw = _git(root, "show", revision + ":" + relative)
            if raw is None:
                continue
            for binding in json.loads(raw):
                identity = binding["id"]
                if identity in result:
                    # An ambiguous registration never supplies an anchor.
                    result[identity] = None
                else:
                    result[identity] = (revision, binding)
    cache[key] = result
    return result


def _function_owners(lines: list[str]) -> list[int | None]:
    declaration = re.compile(
        r"^\s*(?:(?:pub(?:\([^)]*\))?|async|unsafe|const)\s+)*(?:fn|def)\s+[A-Za-z_]\w*\b")
    owner, result = None, []
    for line, text in enumerate(lines, 1):
        if declaration.search(text):
            owner = line
        result.append(owner)
    return result


def _unchanged_lines(old: list[str], current: list[str]) -> dict[int, int]:
    result = {}
    old_text, current_text = "\n".join(old), "\n".join(current)
    for before, after, size in difflib.SequenceMatcher(None, old, current, autojunk=False).get_matching_blocks():
        if size < 3:
            continue
        block = "\n".join(old[before:before + size])
        if old_text.count(block) != 1 or current_text.count(block) != 1:
            continue
        result.update((before + offset + 1, after + offset + 1) for offset in range(size))
    old_owners, current_owners = _function_owners(old), _function_owners(current)
    return {before: after for before, after in result.items()
            if before == after or (old_owners[before - 1] is not None
                and result.get(old_owners[before - 1]) == current_owners[after - 1])}


def location_line(root: Path, ledger: Path, binding: dict, kind: str,
                  location: dict, current: list[str], matches, cache: dict) -> int | None:
    """Keep direct validation for new bindings; map only the exact committed one."""
    line = location.get("line", 0)
    registered = _bindings(root, ledger, cache).get(binding["id"])
    if registered is None:
        return line
    revision, original = registered
    fields = ("name", "relation", "source_locations", "assertion_locations")
    if (any(binding.get(field) != original.get(field) for field in fields)
            or location not in original.get(kind, [])):
        return line
    key = ("source-location-map", revision, location["path"])
    if key not in cache:
        raw = _git(root, "show", revision + ":" + location["path"])
        old = raw.splitlines() if raw is not None else []
        cache[key] = (old, _unchanged_lines(old, current))
    old, mapping = cache[key]
    if not (0 < line <= len(old)) or not matches(old[line - 1]):
        # Legacy bindings without a matching original anchor still have to pass
        # the existing direct source/definition/assertion checks.
        return line
    return mapping.get(line)
