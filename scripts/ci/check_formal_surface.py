#!/usr/bin/env python3
"""Inventory production Rust modules that carry formal-safety risk signals.

The obligation ledger is claim-oriented.  This complementary inventory is
source-oriented: it prevents a manually curated claim list from reporting a
high ratio while whole authorization, state-machine, concurrency, durability,
or secret-handling modules are absent from the denominator discussion.

Two modes, one predicate.  ``--require-complete`` walks the whole tree because
the coverage ratio it prints is global.  ``--staged [paths...]`` asks the same
question -- "does this production source carry a risk signal nobody has
classified or excluded?" -- about explicit paths or the actual staged changes.
Incremental classification and Rust bytes come from the same index; only an explicit
non-repository fixture reads disk. The pre-commit hook
can legitimately trigger on ``crates/**/*.rs`` without a whole-repository walk.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys

import _crate_boundary_workspace
import formal_anchors
import formal_ledger
import formal_sources


# Input set: every production Rust source it inventories, the Cargo manifests
# that decide which of them a target actually reaches, the obligation ledger
# and the three formal/ JSON registries (features, classifications,
# exclusions) with the module that assembles the ledger, and the shared Rust
# projection whose lexer decides what counts as executable vocabulary.
# `crates/**/*.rs` belongs here because the checker reads every one of them --
# the earlier hook globbed only formal/*.json, so two new production modules
# reached main unclassified.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "formal/*.json",
    "formal/coverage.toml",
    "formal/obligations/**/*",
    "web/src/**/*",
    "scripts/ci/_crate_boundary_workspace.py",
    "scripts/ci/formal_ledger.py",
    "scripts/ci/formal_sources.py",
)

ROOT = pathlib.Path(__file__).resolve().parents[2]
FEATURES = ROOT / "formal" / "features.json"


# Each category is a disjunction of conjunctions.  This keeps a lone word in a
# comment or DTO from becoming a safety surface while retaining every concrete
# decision, state transition, synchronization, durable fence, and plaintext
# boundary.  For example, a revision field alone is data; revision + CAS/fence
# logic is a formalizable safety relation.
SIGNAL_GROUPS: dict[str, tuple[tuple[re.Pattern[str], ...], ...]] = {
    "authorization": (
        (re.compile(r"\b(?:authorize|authorization|authz|authenticate)\w*\b", re.I),),
        (re.compile(r"\bpermission\w*\b", re.I),),
        (
            re.compile(r"\b(?:scope|tenant|workspace)[A-Za-z0-9_]*\b", re.I),
            re.compile(r"\b(?:resolve|guard|fence|admit|claim|allow|deny)\w*\b", re.I),
        ),
        (
            re.compile(r"\bvalidate_credential_file_ownership\b"),
            re.compile(r"\bRole::(?:AllInOne|Coordinator|Worker)\b"),
        ),
        (re.compile(r"\bcheck_entitlement\b"),),
        # Registered-Worker identity is an authorization decision even when its
        # temporal-policy enum has no lifecycle/permission keyword. Imports alone
        # are not calls; the shared production projection removes test-only code.
        (re.compile(r"\bverify_current_worker_identity\s*\("),),
        (
            re.compile(r"\bhas_work_session_access\b"),
            re.compile(r"\bWorkSessionAccess\b"),
            re.compile(r"\bWorkspaceScope::non_empty\b"),
        ),
        (
            re.compile(r"\bNetworkPolicy::(?:Unrestricted|Allowlist)\b"),
            re.compile(r"\bEnvironmentNetworking::(?:Unrestricted|Limited)\b"),
        ),
        (
            re.compile(r"\bSha256::new\b"),
            re.compile(r"\bcall_id\.as_bytes\b"),
            re.compile(r"\bwrite_workspace_file\b"),
        ),
        (
            re.compile(r"\bComponent::(?:ParentDir|RootDir|Prefix)\b"),
            re.compile(r"\bfn\s+safe_root\b"),
        ),
        (
            re.compile(r"\bK8S_DNS_(?:LABEL|SUBDOMAIN)_MAX_LEN\b"),
            re.compile(r"\bblake3::hash\s*\(\s*scope\.as_bytes\s*\(\s*\)\s*\)"),
        ),
        (
            re.compile(r"\bSandboxControlBindingRequest::"),
            re.compile(r"\bautomount_service_account_token\b"),
        ),
    ),
    "state_machine": (
        (
            re.compile(
                r"\benum\s+[A-Za-z0-9_]*(?:State|Phase|Status|Outcome|Disposition|Lifecycle)\b"
            ),
        ),
        (re.compile(r"\bfn\s+[A-Za-z0-9_]*(?:transition|next_state|advance|settle)\w*\b", re.I),),
        (
            re.compile(r"\b(?:terminal|absorbing)\w*\b", re.I),
            re.compile(r"\b(?:state|phase|status|outcome)\w*\b", re.I),
        ),
        (
            re.compile(r"\bfn\s+[A-Za-z0-9_]*archive[A-Za-z0-9_]*\b"),
            re.compile(r"\bSessionDisposition::"),
        ),
        (
            re.compile(r"\bTaskStatus::"),
            re.compile(r"\bToolTaskPoll::"),
        ),
        (
            re.compile(r"\benum\s+(?:Result|Status)Matcher\b"),
            re.compile(r"\bfn\s+(?:result_matches|status_match)\b"),
        ),
        (
            re.compile(r"\binstalled_sessions\s*\.\s*update\s*\("),
            re.compile(
                r"\benvironment_owner\s*\.\s*(?:begin_[A-Za-z0-9_]*|publish_prepared)\s*\("
            ),
        ),
    ),
    "concurrency": (
        (re.compile(r"\b(?:Mutex|RwLock|Atomic[A-Za-z0-9_]*|compare_exchange)\b"),),
        (re.compile(r"\b(?:tokio::select|loom::|spawn_blocking|JoinSet)\b"),),
        (
            re.compile(
                r"\bwatch::Receiver\s*<\s*Option\s*<\s*JsonRpcNotifier\s*>\s*>"
            ),
            re.compile(r"\bpublished_notifier\b"),
        ),
        (re.compile(r"\.\s*lock\s*\(\s*\)\s*\.\s*await\b"),),
    ),
    "durability": (
        (
            re.compile(
                r"\b(?:transaction|begin_mutation|outbox|checkpoint|idempoten)\w*\b",
                re.I,
            ),
        ),
        (
            re.compile(r"\b(?:lease|revision|generation)\w*\b", re.I),
            re.compile(r"\b(?:claim|compare|fence|cas|stale|mutat|commit|renew)\w*\b", re.I),
        ),
        (
            re.compile(r"\b(?:recover|retry|reconcile)\w*\b", re.I),
            re.compile(r"\b(?:state|durable|pending|queue|outbox|checkpoint)\w*\b", re.I),
        ),
        (re.compile(r"\b(?:INSERT|UPDATE|DELETE)\s+(?:INTO|FROM)?\b", re.I),),
        (
            re.compile(r"\bexpected_prior_commit\b"),
            re.compile(r"\bverify[A-Za-z0-9_]*\b"),
        ),
        (
            re.compile(r"\breceipt_fingerprint[A-Za-z0-9_]*\b"),
            re.compile(r"\b(?:verify|canonical)[A-Za-z0-9_]*\b"),
            re.compile(r"\bsession_cleanup_completion_admitted\s*\("),
        ),
        (
            re.compile(r"\benum\s+PublishedMemoryStream\b"),
            re.compile(r"\bselected_memory_store_bundle\b"),
        ),
        (
            re.compile(r"\bResourceReferenceIndex\b"),
            re.compile(r"\badd_reference\b"),
            re.compile(r"\bremove_reference\b"),
        ),
        (
            re.compile(r"\bLiveCommand::Cancel\b"),
            re.compile(r"\.\s*cancel\s*\(\s*&run_id\s*\)"),
            re.compile(r"\.\s*tick_run\s*\(\s*&run_id\b"),
        ),
    ),
    "secret_boundary": (
        (re.compile(r"\b(?:PlaintextHolder|PlaintextBoundary)\b"),),
        (
            re.compile(r"\b(?:credential|secret|bearer|token|material)\w*\b", re.I),
            re.compile(
                r"\b(?:resolve|materializ|validate|rotate|revoke|redact|inject|holder|boundary)\w*\b",
                re.I,
            ),
        ),
        (
            re.compile(r"\bstruct\s+RedactedString\b"),
            re.compile(r"\b(?:expose_secret|zeroize)\b"),
        ),
        (
            re.compile(r"\bfn\s+http_basic_material\b"),
            re.compile(r"\bStructuredCredentialMaterial\b"),
            re.compile(r"\bRedactedString\b"),
        ),
        (re.compile(r"\bSealedAeadSecretStore::over\b"),),
        (
            re.compile(r"\bCredentialArtifactCodec\b"),
            re.compile(r"\.\s*expose_secret\s*\("),
        ),
        (
            re.compile(r"\bclassify_secret_store_error\b"),
            re.compile(r"\bCredentialMaterialError::(?:Invalid|Unavailable)\b"),
        ),
        (
            re.compile(r"\bproject_work_with_secret\b"),
            re.compile(r"\.\s*expose_secret\s*\("),
        ),
    ),
}

PRODUCTION_ROOTS = (
    ROOT / "crates" / "contract",
    ROOT / "crates" / "control",
    ROOT / "crates" / "resources",
    ROOT / "crates" / "runtime",
    ROOT / "crates" / "server",
    ROOT / "crates" / "stores",
    ROOT / "crates" / "worker",
    ROOT / "crates" / "bin",
)

FORMAL_EVIDENCE_STATUSES = {"model_linked", "kernel_proved"}


def fail(message: str) -> None:
    print(f"formal surface: {message}", file=sys.stderr)
    raise SystemExit(1)


def is_production_source(path: pathlib.Path) -> bool:
    relative = path.relative_to(ROOT)
    parts = relative.parts
    if "src" not in parts:
        return False
    if path.name in {"tests.rs", "test.rs", "formal.rs"} or path.name.endswith(
        "_tests.rs"
    ):
        return False
    return not any(part in {"tests", "test_support", "fixtures"} for part in parts)


# A `mod`/`use`/`extern crate` item states where code lives, not what this file
# decides.  `crates/contract/awaken-agent-contract/src/audit/mod.rs` is six
# `pub mod` lines and nothing else, yet it matched the `authorization` signal
# purely because one declared module is named `permission`.  The head of such an
# item is therefore blanked before matching.  The optional attribute prefix is
# consumed too, so `#[cfg(feature = "x")] pub mod permission;` cannot smuggle the
# same name back in.
DECLARATION_HEAD = re.compile(
    r"(?:(?<=\n)|(?<=;)|(?<=\{)|(?<=\})|\A)"
    r"[ \t]*(?:\#\[[^\]]*\][ \t]*)*"
    r"((?:pub(?:[ \t]*\([^)]*\))?[ \t]+)?(?:mod|use|extern[ \t]+crate)\b)"
)


def without_declarations(code: str) -> str:
    """Blank module/import declaration heads, preserving offsets and lines.

    ``use a::{b, c};`` is blanked through its closing ``;`` including the braced
    group, so a multi-line import cannot leak its names.  An inline ``mod name {``
    keeps its *body* -- only the declaration head up to the brace is blanked,
    because the items inside it are this file's executable vocabulary.

    String literals are deliberately left alone: several signal groups exist to
    catch behaviour that only appears inside a literal (SQL ``INSERT INTO`` is
    the clearest case), so blanking literals would delete real surfaces.  Line
    and block comments are already blanked by the shared production projection
    this runs after.
    """

    projected = list(code)
    cursor = 0
    while (match := DECLARATION_HEAD.search(code, cursor)) is not None:
        keyword = match.group(1).split()[-1]
        index = match.end(1)
        depth = 0
        end = len(code)
        while index < len(code):
            character = code[index]
            if character == "{":
                if keyword == "mod" and depth == 0:
                    end = index
                    break
                depth += 1
            elif character == "}":
                depth -= 1
            elif character == ";" and depth == 0:
                end = index + 1
                break
            index += 1
        span = code[match.start():end]
        projected[match.start():end] = (
            character if character in "\r\n" else " " for character in span
        )
        cursor = max(end, match.end(1))
    return "".join(projected)


def detect_signals(source: str) -> list[str]:
    code = without_declarations(_crate_boundary_workspace.production_rust(source))
    return [
        category
        for category, groups in SIGNAL_GROUPS.items()
        if any(all(pattern.search(code) for pattern in group) for group in groups)
    ]


def surface_anchors() -> dict[str, formal_anchors.Surface]:
    """Each Rust source's own classification, from its `// @formal surface` lines."""

    headers, errors = formal_anchors.headers(ROOT)
    classified, surface_errors = formal_anchors.surfaces(headers)
    errors += surface_errors
    if errors:
        fail("\n".join(sorted(errors)))
    return {path: surface for path, surface in classified.items() if path.endswith(".rs")}


def load_exclusions() -> dict[str, dict[str, object]]:
    exclusions: dict[str, dict[str, object]] = {}
    existing = set(formal_sources.tracked_files(ROOT, allow_nonrepository=True))
    for path, surface in surface_anchors().items():
        if surface.exclude is None:
            continue
        missing = [owner for owner in surface.owners if owner not in existing]
        if missing:
            fail(f"{surface.path}:{surface.line}: surface exclusion names missing owner(s) {', '.join(missing)}")
        exclusions[path] = {
            "path": path, "boundary": surface.exclude, "reason": surface.reason,
            "evidence": list(surface.owners),
        }
    return exclusions


def load_requirement_ids() -> set[str]:
    relative = FEATURES.relative_to(ROOT).as_posix()
    text = formal_sources.file_text(
        ROOT, relative, staged=formal_sources.repository_environment(ROOT) is not None,
    )
    if text is None:
        raise formal_sources.SourceError(f"Missing formal requirement input: {relative}")
    raw = json.loads(text)
    requirement_ids: set[str] = set()
    for feature in raw.get("features", []):
        feature_id = str(feature.get("id", ""))
        for requirement in feature.get("requirements", []):
            local_id = str(requirement.get("id", ""))
            requirement_id = f"{feature_id}.{local_id}"
            if not feature_id or not local_id or requirement_id in requirement_ids:
                fail(f"duplicate or empty product requirement id {requirement_id!r}")
            requirement_ids.add(requirement_id)
    return requirement_ids


def load_classifications(
    obligation_ids: set[str], requirement_ids: set[str]
) -> dict[str, dict[str, object]]:
    classifications: dict[str, dict[str, object]] = {}
    for path, surface in surface_anchors().items():
        if surface.exclude is not None:
            continue
        if surface.obligations:
            missing = sorted(set(surface.obligations) - obligation_ids)
            if missing:
                fail(f"surface classification {path} names missing obligations: " + ", ".join(missing))
            classifications[path] = {"path": path, "obligation_ids": list(surface.obligations),
                                     "reason": surface.reason}
        else:
            if surface.requirement not in requirement_ids:
                fail(
                    f"surface classification {path} names missing complete product "
                    f"requirement id {surface.requirement!r}"
                )
            classifications[path] = {"path": path, "requirement_id": surface.requirement,
                                     "reason": surface.reason}
    return classifications


def ledger_obligations() -> list[dict[str, object]]:
    """The assembled obligation rows of the tree under ``ROOT``."""

    try:
        return formal_ledger.load(ROOT)["obligations"]
    except formal_ledger.LedgerError as error:
        fail(str(error))


def ledger_sources() -> dict[str, list[dict[str, object]]]:
    """Production Rust sources an obligation already cites as evidence."""

    by_source: dict[str, list[dict[str, object]]] = {}
    for obligation in ledger_obligations():
        for evidence in obligation.get("evidence", []):
            if evidence.startswith("crates/") and evidence.endswith(".rs"):
                by_source.setdefault(evidence, []).append(obligation)
    return by_source


def duplicate_authority_paths(
    by_source: dict[str, list[dict[str, object]]],
    exclusions: dict[str, dict[str, object]],
    classifications: dict[str, dict[str, object]],
) -> list[str]:
    """The same single-authority rule for whole-tree and selected-source gates."""
    return sorted(
        path for path in classifications
        if path in exclusions or bool(by_source.get(path))
    )


def under_production_root(path: pathlib.Path) -> bool:
    """Is this path inside one of the roots the whole-tree inventory walks?"""

    return any(root == path or root in path.parents for root in PRODUCTION_ROOTS)


def check_staged(paths: list[str]) -> None:
    """Apply the existing surface predicate to one selected index subject.

    Explicit paths retain their scope. With no paths, the canonical Git reader
    selects changed index entries; absence is never an implicit empty argument
    list. Classification, requirements, owners and risk signals use index bytes
    together. Only an explicitly selected non-repository fixture reads disk.
    """
    try:
        staged = formal_sources.repository_environment(ROOT) is not None
        if not paths:
            paths = formal_sources.git_paths(
                ROOT, "diff", "--cached", "--name-only", "--diff-filter=ACMRT", "-z", "--",
            )
            staged = True
        elif not staged and ((ROOT / ".git").exists() or (ROOT / ".git").is_symlink()):
            raise formal_sources.SourceError("The repository index cannot be selected")
        indexed = set(formal_sources.tracked_files(ROOT)) if staged else set()
        deleted = set(formal_sources.git_paths(
            ROOT, "diff", "--cached", "--no-renames", "--name-only", "--diff-filter=D", "-z", "--",
        )) if staged else set()
        by_source = ledger_sources()
        exclusions = load_exclusions()
        classifications = load_classifications(
            {item["id"] for item in ledger_obligations()}, load_requirement_ids(),
        )
        registered = set(by_source) | set(exclusions) | set(classifications)
        duplicate_authorities = set(duplicate_authority_paths(
            by_source, exclusions, classifications,
        ))
        unclassified: list[tuple[str, list[str]]] = []
        checked = 0
        for argument in paths:
            path = pathlib.Path(argument)
            path = path if path.is_absolute() else ROOT / path
            if path.suffix != ".rs":
                continue
            try:
                relative = path.resolve().relative_to(ROOT).as_posix()
            except ValueError:
                raise formal_sources.SourceError(f"Selected Rust path is outside the source root: {argument}")
            candidate = ROOT / relative
            if not under_production_root(candidate) or not is_production_source(candidate):
                continue
            if staged and relative not in indexed:
                if relative in deleted:
                    continue
                raise formal_sources.SourceError(f"Selected Rust path is absent from the index: {relative}")
            source = formal_sources.file_text(ROOT, relative, staged=staged)
            if source is None:
                continue
            checked += 1
            if relative in duplicate_authorities:
                fail("classifications duplicate an exclusion or source-ledger link: " + relative)
            if relative in registered:
                continue
            signals = detect_signals(source)
            if signals:
                unclassified.append((relative, signals))
        if unclassified:
            fail(
                "staged production sources carry unclassified safety surfaces; classify each in "
                "its own opening lines (`// @formal surface requirement=|obligation=|exclude=` "
                "with a `// @formal reason:`):\n"
                + "\n".join(f"  {relative} [{','.join(signals)}]" for relative, signals in unclassified)
            )
        subject = "index" if staged else "explicit disk fixture"
        print(f"formal source surface: {checked} production Rust candidate(s) checked "
              f"({len(paths)} selected path(s), source={subject}), 0 unclassified")
    except (OSError, subprocess.CalledProcessError, formal_sources.SourceError) as error:
        fail(f"Cannot read staged formal source: {error}")


def main() -> None:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument(
        "--require-complete",
        action="store_true",
        help="fail when a detected production surface has no obligation or exclusion",
    )
    parser.add_argument(
        "--staged",
        nargs="*",
        metavar="PATH",
        help="check index sources at these paths; without paths select actual staged changes (O(changed))",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit the complete inventory as JSON",
    )
    args = parser.parse_args()

    if args.staged is not None:
        check_staged(args.staged)
        return

    obligations_by_id = {item["id"]: item for item in ledger_obligations()}
    by_source = ledger_sources()

    exclusions = load_exclusions()
    classifications = load_classifications(
        set(obligations_by_id), load_requirement_ids()
    )
    # Evidence must be compiled by some build. A Kani proof module is compiled
    # by Kani and never shipped, so the production graph alone would call it
    # detached; the compiled graph follows test and kani branches as well.
    reachable = _crate_boundary_workspace.compiled_module_files(
        _crate_boundary_workspace.iter_crate_manifests()
    )
    detached = sorted(
        relative
        for relative in set(by_source) | set(classifications) | set(exclusions)
        if is_production_source(ROOT / relative)
        and (ROOT / relative).resolve() not in reachable
        and not (
            exclusions.get(relative, {}).get("boundary") == "test_only"
            and relative not in by_source
            and relative not in classifications
        )
    )
    if detached:
        fail("registered production evidence is detached from Cargo module roots: " + ", ".join(detached))
    inventory: list[dict[str, object]] = []
    for root in PRODUCTION_ROOTS:
        for path in root.glob("**/*.rs"):
            if not is_production_source(path):
                continue
            source = path.read_text(encoding="utf-8")
            signals = detect_signals(source)
            if not signals:
                continue
            relative = path.relative_to(ROOT).as_posix()
            source_obligations = by_source.get(relative, [])
            classification = classifications.get(relative)
            classified_obligation_ids = (
                classification.get("obligation_ids", []) if classification else []
            )
            obligations = source_obligations + [
                obligations_by_id[item_id] for item_id in classified_obligation_ids
            ]
            product_requirement = (
                classification.get("requirement_id") if classification else None
            )
            formally_linked = [
                item["id"]
                for item in obligations
                if item["status"] in FORMAL_EVIDENCE_STATUSES
            ]
            directly_proved = [
                item["id"]
                for item in obligations
                if item["status"] == "kernel_proved"
                or item.get("proof_harnesses")
                or "crates/runtime/awaken-runtime/tests/formal_refinement.rs"
                in item.get("evidence", [])
            ]
            inventory.append(
                {
                    "path": relative,
                    "signals": signals,
                    "obligations": [item["id"] for item in obligations],
                    "formal_evidence_obligations": formally_linked,
                    "direct_proof_obligations": directly_proved,
                    "product_requirement": product_requirement,
                    "classification": classification,
                    "excluded": exclusions.get(relative),
                }
            )

    inventory.sort(key=lambda item: str(item["path"]))
    paths = {str(item["path"]) for item in inventory}
    # A classified source whose signal heuristic stopped matching is reported,
    # not refused: the classification lives in the file, so it moves and dies
    # with it, and a regex drifting under a reviewed decision is not a defect
    # of the commit that edited the file.
    stale = sorted((set(exclusions) | set(classifications)) - paths)
    redundant_classifications = duplicate_authority_paths(
        by_source, exclusions, classifications,
    )
    if redundant_classifications:
        fail(
            "classifications duplicate an exclusion or source-ledger link: "
            + ", ".join(redundant_classifications)
        )

    classified = [
        item
        for item in inventory
        if item["obligations"]
        or item["product_requirement"] is not None
        or item["excluded"] is not None
    ]
    formal_evidence_linked = [
        item for item in inventory if item["formal_evidence_obligations"]
    ]
    direct_proof_linked = [
        item for item in inventory if item["direct_proof_obligations"]
    ]
    product_requirement_linked = [
        item for item in inventory if item["product_requirement"] is not None
    ]
    uncovered = [
        item
        for item in inventory
        if not item["obligations"]
        and item["product_requirement"] is None
        and item["excluded"] is None
    ]
    classified_ratio = len(classified) / len(inventory) if inventory else 1.0
    formal_evidence_ratio = (
        len(formal_evidence_linked) / len(inventory) if inventory else 1.0
    )
    direct_proof_ratio = len(direct_proof_linked) / len(inventory) if inventory else 1.0
    product_requirement_ratio = (
        len(product_requirement_linked) / len(inventory) if inventory else 1.0
    )

    if args.json:
        print(
            json.dumps(
                {
                    "summary": {
                        "inventoried": len(inventory),
                        "classified": len(classified),
                        "formal_evidence_linked": len(formal_evidence_linked),
                        "direct_proof_linked": len(direct_proof_linked),
                        "product_requirement_linked": len(product_requirement_linked),
                        "uncovered": len(uncovered),
                        "classified_ratio": classified_ratio,
                        "formal_evidence_linked_ratio": formal_evidence_ratio,
                        "direct_proof_linked_ratio": direct_proof_ratio,
                        "product_requirement_linked_ratio": product_requirement_ratio,
                    },
                    "surfaces": inventory,
                    "stale_classifications": stale,
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print(
            "formal source surface: "
            f"{len(classified)}/{len(inventory)} classified ({classified_ratio:.1%}); "
            f"{len(formal_evidence_linked)}/{len(inventory)} formal-evidence-linked "
            f"({formal_evidence_ratio:.1%}); "
            f"{len(direct_proof_linked)}/{len(inventory)} direct-proof-linked "
            f"({direct_proof_ratio:.1%}); "
            f"{len(product_requirement_linked)}/{len(inventory)} product-requirement-linked "
            f"({product_requirement_ratio:.1%}); "
            f"{len(uncovered)} uncovered"
        )
        for relative in stale:
            print(f"  report: {relative} is classified but no longer carries a risk signal")
        for item in uncovered[:25]:
            print(f"  uncovered {item['path']} [{','.join(item['signals'])}]")
        if len(uncovered) > 25:
            print(f"  ... and {len(uncovered) - 25} more")

    if args.require_complete and uncovered:
        fail(f"{len(uncovered)} detected production safety surfaces are unclassified")


if __name__ == "__main__":
    main()
