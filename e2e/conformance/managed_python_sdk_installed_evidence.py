from __future__ import annotations

import json
import sys
from pathlib import Path


REPO = Path(__file__).resolve().parents[2]
ORACLE_IMPLEMENTATION = REPO / "packages/managed-sdk-oracle/python"
sys.path.insert(0, str(ORACLE_IMPLEMENTATION))
import oracle as oracle_generator  # noqa: E402 - repository-owned extractor


def assert_installed_evidence(
    version: str,
    installed_root: Path,
    oracle_path: Path,
    scope_path: Path,
) -> tuple[dict[str, object], dict[str, object]]:
    # Evidence graph: C1=an exact digest-qualified wheel; C2=its operation,
    # helper, export, handwritten runtime, SSE, and transitive DTO sources are
    # re-extracted from the installed bytes; C3=one generated oracle owns the
    # expected fingerprints. E1=every fingerprint and count agrees before the
    # wheel can issue a compatibility request. The current oracle additionally
    # compares every path/hash row; historical summaries retain one fingerprint
    # and count instead of copying thousands of generated rows per version.
    expected_oracle = json.loads(oracle_path.read_text(encoding="utf-8"))
    current = expected_oracle["current"]
    expected = current if current["version"] == version else next(
        item for item in expected_oracle["anchors"] if item["version"] == version
    )
    scope = json.loads(scope_path.read_text(encoding="utf-8"))
    evidence = oracle_generator.extract(installed_root, version, scope)
    fields = (
        "operation_fingerprint",
        "source_fingerprint",
        "source_file_count",
        "helper_fingerprint",
        "helpers",
        "library_export_fingerprint",
        "library_exports",
        "type_source_fingerprint",
        "runtime_source_fingerprint",
        "runtime_sources",
        "stream_event_fingerprint",
        "stream_event_names",
    )
    for field in fields:
        assert evidence[field] == expected[field], f"{version}: installed {field}"
    if "type_sources" in expected:
        assert evidence["type_sources"] == expected["type_sources"], (
            f"{version}: installed type source closure"
        )
    assert len(evidence["operations"]) == expected.get(
        "operation_count", len(expected.get("operations", []))
    )
    assert len(evidence["library_exports"]) == expected.get(
        "library_export_count", len(expected.get("library_exports", []))
    )
    assert len(evidence["type_sources"]) == expected.get(
        "type_source_count", len(expected.get("type_sources", []))
    )
    return evidence, expected
