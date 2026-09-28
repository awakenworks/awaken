"""The formal obligation ledger, read through one owner.

Every gate that reasons about formal obligations -- the coverage, surface,
feature, lane-reachability and proof-input checkers, the verified-profile
qualification and the release manifest -- consumes the same assembled value:
the coverage ledger in its v1 shape,

    {metric, minimum_ratio, verified_profile, obligations: [...]}

where each obligation carries ``id``, ``formalizable``, ``status``,
``evidence``, ``proof_harnesses``, ``claim_scope``, ``proof_grade``,
``bounds``, ``tcb_assumptions`` and ``production_entrypoints``.

The consumers never read the storage themselves. That is the point of this
module: the storage is allowed to change shape (one file today; one file per
obligation plus source anchors next) without any consumer noticing, and the
equivalence of two storages is one comparison of ``canonical_bytes(load())``.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

# Consumer: every formal gate and the release manifest, through ``load``.
STORAGE = "formal/coverage.json"


class LedgerError(ValueError):
    """The stored ledger cannot be assembled into the v1 shape."""


def present(root: Path) -> bool:
    """Does ``root`` carry an obligation ledger at all?"""

    return (root / STORAGE).is_file()


def load(root: Path) -> dict:
    """Assemble the coverage ledger of ``root`` in its v1 shape."""

    path = root / STORAGE
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise LedgerError(f"cannot read {STORAGE}: {error}") from error
    if not isinstance(value, dict) or not isinstance(value.get("obligations"), list):
        raise LedgerError(f"{STORAGE} has no obligation list")
    return value


def write(root: Path, ledger: dict) -> None:
    """Store ``ledger`` (v1 shape) under ``root`` in the current storage form.

    Consumers: checker self-tests that build a fixture tree, so a fixture
    follows the storage instead of hard-coding it.
    """

    path = root / STORAGE
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")


def canonical_bytes(ledger: dict) -> bytes:
    """Order-independent bytes of an assembled ledger.

    Obligations are ordered by id; every other list keeps its order, because a
    list's order is part of what was declared.
    """

    ordered = dict(ledger)
    ordered["obligations"] = sorted(ledger["obligations"], key=lambda row: row["id"])
    return (json.dumps(ordered, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def digest(ledger: dict) -> str:
    return "sha256:" + hashlib.sha256(canonical_bytes(ledger)).hexdigest()
