"""Refuse a wire type that accepts input it does not model.

`#[serde(deny_unknown_fields)]` is a per-container opt-in, and two properties of
it produce the same defect over and over:

  1. It does not recurse. A closed request body made of open field types is
     open. `ModelSelectionWire` carried the attribute *and* an eight-line
     comment about the hazard, while the `ModelTarget` it wraps carried none,
     so `{"model_id":"gpt-5",…,"endpoint":"eu-primary"}` decoded with
     `endpoint_name: None` and the authored endpoint was gone.
  2. It does not govern a *unit* variant of an internally tagged enum. Serde
     reads such a variant's tag and discards every sibling key, so the attribute
     sitting right above the variant closes its struct siblings and nothing
     else. `{"type":"unrestricted","allowed_hosts":[…]}` was accepted as
     unrestricted egress with a 200.

Property 2 is the more misleading half — the attribute is present, so the type
reads as closed — and it is what this gate refuses outright.

For property 1 the gate takes the decode point that is both unambiguous and the
one an author reaches for first: a type named by an axum `Json`/`Query`/`Form`
extractor in a handler signature is, by construction, decoded from a body
someone outside this process wrote. Types reachable only through *fields* of
those roots are the larger population and a migration, not a gate; they are
held by the ratchet below rather than frozen in a file list.

Whole-repo scan plus an exemption registry, never a file allow-list: a list
freezes today's instances, a scan also refuses tomorrow's. Each registry entry
is a decision someone made and can be argued with.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


# Key: the request-body type. Value: why it must keep accepting keys it does not
# model. Closing a live request body is a breaking change for a client that
# already sends an extra field, so "a real client sends this" is a valid entry
# — with the field named, so a later reader can retire it.
OPEN_REQUEST_BODY_EXEMPTIONS: dict[str, str] = {}

# Key: "<enum>::<variant>". Value: why a bare unit variant is correct here
# despite the enum's own `deny_unknown_fields`.
ESCAPING_UNIT_VARIANT_EXEMPTIONS: dict[str, str] = {}

# Both shapes predate the gate. Converting every instance at once would be a
# migration, not a fix, so the untriaged remainder is ratcheted: the count may
# fall and may not rise. That is what makes this gate refuse a *new* open
# request body today rather than only after the migration finishes — the
# failure mode of a file allow-list, which freezes the debt and guards nothing.
# Lower these as instances are closed or registered; never raise them.
OPEN_REQUEST_BODY_BASELINE = 43
ESCAPING_UNIT_VARIANT_BASELINE = 20

# `Json(body): Json<T>` in a handler signature. The `let` guard matters: sqlx
# spells a stored JSON column `let Json(row): Json<T> = …`, which is this
# process reading back bytes it wrote, not an externally authored body.
EXTRACTOR = re.compile(
    r"(?:Json|Query|Form)\(\s*[A-Za-z_]\w*\s*\)\s*:\s*"
    r"(?:axum::extract::)?(?:Json|Query|Form)\s*<\s*([A-Za-z_][\w:]*)"
)
DERIVE = re.compile(r"#\[derive\s*\(")
DECLARATION = re.compile(r"\s*(?:pub(?:\([^)]*\))?\s+)?(struct|enum)\s+([A-Za-z_]\w*)")
NAMED_FIELD = re.compile(
    r"[{,]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?[a-z_]\w*\s*:\s*\S"
)
INTERNALLY_TAGGED = re.compile(r"serde\s*\(\s*[^)]*\btag\s*=")
UNIT_VARIANT = re.compile(r"^[A-Z]\w*$")

# Extractor type arguments that are not a modelled wire type at all.
OPAQUE_EXTRACTOR_ARGUMENTS = {"Value", "String", "HashMap", "BTreeMap", "T"}


def _bracket_end(source: str, open_index: int, opener: str, closer: str) -> int:
    depth = 0
    index = open_index
    while index < len(source):
        if source[index] == opener:
            depth += 1
        elif source[index] == closer:
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return len(source) - 1


def _attribute_end(source: str, hash_index: int) -> int:
    return _bracket_end(source, source.index("[", hash_index), "[", "]") + 1


def _split_top_level(source: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    current: list[str] = []
    for character in source:
        if character in "<([{":
            depth += 1
        elif character in ">)]}":
            depth -= 1
        if character == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(character)
    parts.append("".join(current))
    return parts


def deserialize_items(production: str) -> list[dict]:
    """Every `#[derive(…Deserialize…)]` struct/enum, with its attribute run."""
    items: list[dict] = []
    for derive in DERIVE.finditer(production):
        end = _attribute_end(production, derive.start())
        if "Deserialize" not in production[derive.start() : end]:
            continue
        attributes = [production[derive.start() : end]]
        cursor = end
        while re.compile(r"\s*#\[").match(production, cursor) is not None:
            hash_index = production.index("#", cursor)
            cursor = _attribute_end(production, hash_index)
            attributes.append(production[hash_index:cursor])
        declaration = DECLARATION.match(production, cursor)
        if declaration is None:
            continue
        body_start = production.find("{", declaration.end() - 1)
        semicolon = production.find(";", declaration.end() - 1)
        if body_start < 0 or 0 <= semicolon < body_start:
            body = ""
        else:
            body = production[body_start : _bracket_end(production, body_start, "{", "}") + 1]
        items.append(
            {
                "kind": declaration.group(1),
                "name": declaration.group(2),
                "attributes": "\n".join(attributes),
                "body": body,
                "line": production[: derive.start()].count("\n") + 1,
            }
        )
    return items


def _production_sources(crates: Path) -> dict[Path, str]:
    return {
        path: _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for path in sorted(crates.glob("*/*/src/**/*.rs"))
    }


def _extractor_roots(sources: dict[Path, str]) -> set[str]:
    roots: set[str] = set()
    for production in sources.values():
        for match in EXTRACTOR.finditer(production):
            preceding = production[max(0, match.start() - 16) : match.start()]
            if re.search(r"\blet\s+(?:mut\s+)?$", preceding):
                continue  # a stored JSON column, not an externally authored body
            name = match.group(1).split("::")[-1]
            if name not in OPAQUE_EXTRACTOR_ARGUMENTS:
                roots.add(name)
    return roots


def open_request_bodies(sources: dict[Path, str], repo_root: Path) -> list[tuple[str, str]]:
    """(type, message) for each extractor root that accepts keys it does not model."""
    roots = _extractor_roots(sources)
    found: list[tuple[str, str]] = []
    for path, production in sources.items():
        for item in deserialize_items(production):
            if item["kind"] != "struct" or item["name"] not in roots:
                continue
            if not NAMED_FIELD.search(item["body"]):
                continue  # newtype or unit: there is no unknown field to deny
            if "deny_unknown_fields" in item["attributes"]:
                continue
            if "flatten" in item["body"]:
                continue  # serde forbids the attribute beside `flatten`
            relative = path.relative_to(repo_root)
            found.append(
                (
                    item["name"],
                    f"{relative}:{item['line']}: {item['name']} is decoded from an HTTP body by an "
                    "axum extractor but has no #[serde(deny_unknown_fields)], so a key it does not "
                    "model — a domain name where the wire name differs, a field a later revision "
                    "renamed — is accepted and the authored value is silently dropped. Add the "
                    "attribute, model the field, or record in OPEN_REQUEST_BODY_EXEMPTIONS which "
                    "client sends the extra key"
                ),
            )
    return found


def escaping_unit_variants(sources: dict[Path, str], repo_root: Path) -> list[tuple[str, str]]:
    """(key, message) for each unit variant that escapes its own enum's attribute."""
    found: list[tuple[str, str]] = []
    for path, production in sources.items():
        for item in deserialize_items(production):
            if item["kind"] != "enum" or not item["body"]:
                continue
            attributes = item["attributes"]
            if "deny_unknown_fields" not in attributes:
                continue  # the enum makes no claim this variant could falsify
            if not INTERNALLY_TAGGED.search(attributes) or "untagged" in attributes:
                continue
            if re.search(r"serde\s*\(\s*[^)]*\bcontent\s*=", attributes):
                continue  # adjacently tagged: the outer object is governed
            for part in _split_top_level(item["body"][1:-1]):
                variant = re.sub(r"#\[[^\]]*\]", " ", part).strip()
                if not UNIT_VARIANT.match(variant):
                    continue
                relative = path.relative_to(repo_root)
                found.append(
                    (
                        f"{item['name']}::{variant}",
                        f"{relative}:{item['line']}: {item['name']}::{variant} is a bare unit "
                        f"variant of an internally tagged enum that carries "
                        f"#[serde(deny_unknown_fields)]. Serde reads the tag and discards every "
                        f"sibling key, so the attribute governs this variant's struct siblings and "
                        f"not this variant — the type reads as closed and is not. Write "
                        f"`{variant} {{}}`, which encodes identically and is governed, or record "
                        "in ESCAPING_UNIT_VARIANT_EXEMPTIONS why the keys may be discarded"
                    ),
                )
    return found


def _ratcheted(
    found: list[tuple[str, str]],
    exemptions: dict[str, str],
    baseline: int,
    label: str,
    registry: str,
) -> list[str]:
    live = [message for key, message in found if key not in exemptions]
    seen = {key for key, _ in found}
    errors = [
        f"{registry} names {key}, which no longer holds; drop the stale exemption"
        for key in sorted(exemptions)
        if key not in seen
    ]
    if len(live) > baseline:
        errors.extend(sorted(live))
        errors.append(
            f"{len(live)} {label}, baseline {baseline}: {len(live) - baseline} new one(s). "
            f"Close the type, or record the decision in {registry}."
        )
    elif len(live) < baseline:
        errors.append(
            f"{len(live)} {label}, below the baseline of {baseline}. Lower the baseline to "
            f"{len(live)} so the ground gained is held."
        )
    return errors


def check_all(repo_root: Path, crates: Path) -> list[str]:
    sources = _production_sources(crates)
    return _ratcheted(
        open_request_bodies(sources, repo_root),
        OPEN_REQUEST_BODY_EXEMPTIONS,
        OPEN_REQUEST_BODY_BASELINE,
        "open HTTP request bodies",
        "OPEN_REQUEST_BODY_EXEMPTIONS",
    ) + _ratcheted(
        escaping_unit_variants(sources, repo_root),
        ESCAPING_UNIT_VARIANT_EXEMPTIONS,
        ESCAPING_UNIT_VARIANT_BASELINE,
        "unit variants escaping their enum's deny_unknown_fields",
        "ESCAPING_UNIT_VARIANT_EXEMPTIONS",
    )


def selftest() -> None:
    """Cause/effect over one synthetic crate, so the table is the specification.

    Request bodies. Causes: C1 the type is an axum extractor argument;
    C2 it carries `deny_unknown_fields`; C3 it contains `#[serde(flatten)]`,
    which serde forbids the attribute beside; C4 it is destructured from a
    stored JSON column (`let Json(x): Json<T>`) rather than a handler signature;
    C5 it has no named fields. Effects: E1 reported, E2 not reported.

    | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    | B1   | T  | F  | F  | F  | F  | E1     |
    | B2   | T  | T  | F  | F  | F  | E2     |
    | B3   | T  | F  | T  | F  | F  | E2     |
    | B4   | F  | F  | F  | T  | F  | E2     |
    | B5   | T  | F  | F  | F  | T  | E2     |

    Unit variants. Causes: D1 the enum is internally tagged; D2 it carries
    `deny_unknown_fields`; D3 the variant is bare (no `{}`); D4 the enum is
    adjacently tagged (`content = …`); D5 the declaration is test-only.

    | Rule | D1 | D2 | D3 | D4 | D5 | Effect |
    | V1   | T  | T  | T  | F  | F  | E1     |
    | V2   | T  | T  | F  | F  | F  | E2     |
    | V3   | T  | F  | T  | F  | F  | E2     |
    | V4   | T  | T  | T  | T  | F  | E2     |
    | V5   | T  | T  | T  | F  | T  | E2     |
    """
    import tempfile

    def sources(text: str) -> dict[Path, str]:
        return {Path("crates/server/example/src/lib.rs"): _crate_boundary_workspace.production_rust(text)}

    handler = (
        "async fn create(Json(body): Json<Body>) -> R { todo!() }\n"
        "#[derive(Deserialize)]\npub struct Body { name: String }\n"
    )
    root = Path(".")
    assert [key for key, _ in open_request_bodies(sources(handler), root)] == ["Body"], "B1"

    closed = handler.replace("#[derive(Deserialize)]", "#[derive(Deserialize)]\n#[serde(deny_unknown_fields)]")
    assert not open_request_bodies(sources(closed), root), "B2"

    flattened = handler.replace("{ name: String }", "{ #[serde(flatten)] rest: Extra }")
    assert not open_request_bodies(sources(flattened), root), "B3"

    stored = handler.replace(
        "async fn create(Json(body): Json<Body>) -> R { todo!() }",
        "fn load(row: Row) { let Json(body): Json<Body> = row.get(0); }",
    )
    assert not open_request_bodies(sources(stored), root), "B4"

    newtype = handler.replace("pub struct Body { name: String }", "pub struct Body(String);")
    assert not open_request_bodies(sources(newtype), root), "B5"

    tagged = (
        "#[derive(Deserialize)]\n"
        '#[serde(tag = "type", deny_unknown_fields)]\n'
        "pub enum Networking { Unrestricted, Limited { hosts: Vec<String> } }\n"
    )
    assert [key for key, _ in escaping_unit_variants(sources(tagged), root)] == [
        "Networking::Unrestricted"
    ], "V1"
    assert not escaping_unit_variants(sources(tagged.replace("Unrestricted,", "Unrestricted {},")), root), "V2"
    assert not escaping_unit_variants(sources(tagged.replace(", deny_unknown_fields", "")), root), "V3"
    assert not escaping_unit_variants(
        sources(tagged.replace('tag = "type"', 'tag = "type", content = "data"')), root
    ), "V4"
    assert not escaping_unit_variants(sources("#[cfg(test)]\n" + tagged), root), "V5"

    # The ratchet itself: a count above the baseline fails, a count below it
    # fails too (so ground gained is held), an exemption removes an instance,
    # and an exemption that names nothing live is stale.
    found = [("A", "a"), ("B", "b")]
    assert _ratcheted(found, {}, 1, "x", "REG"), "ratchet: above baseline rejects"
    assert not _ratcheted(found, {}, 2, "x", "REG"), "ratchet: at baseline admits"
    assert _ratcheted(found, {}, 3, "x", "REG"), "ratchet: below baseline demands a lower number"
    assert not _ratcheted(found, {"A": "why"}, 1, "x", "REG"), "ratchet: an exemption removes one"
    assert _ratcheted(found, {"C": "why"}, 2, "x", "REG"), "ratchet: a stale exemption rejects"

    with tempfile.TemporaryDirectory() as directory:
        crates = Path(directory) / "crates"
        source = crates / "server/example/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text(handler + tagged, encoding="utf-8")
        assert check_all(Path(directory), crates), "an unregistered instance fails check_all"

    print("_wire_closure_fitness selftest: the decision tables hold")
