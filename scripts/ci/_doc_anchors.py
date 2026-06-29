"""Shared Markdown anchor resolution for the doc guardrails.

Both ``check_doc_links.py`` and ``check_wiki_okf.py`` need to decide whether a
relative link's optional ``#fragment`` resolves to a heading in the target file.
Keeping the slug rules in one place stops the two checkers from drifting apart.
"""

from __future__ import annotations

import re
from pathlib import Path

HEADING = re.compile(r"^#{1,6}\s+(.*)$")
EXPLICIT_ID = re.compile(r"\{#([^}]+)\}")
FENCE = re.compile(r"^\s*(```|~~~)")

# Resolved-path string -> anchor set (or None if unreadable). Cached per process.
_anchor_cache: dict[str, set[str] | None] = {}


def mask_code_fences(text: str) -> str:
    """Blank out fenced code blocks while preserving character offsets.

    Lines inside ```/~~~ fences (and the fence lines themselves) become equal
    length runs of spaces, so headings shown as examples are not mistaken for
    real ones, yet slice offsets into the original text still line up.
    """
    out: list[str] = []
    in_fence = False
    for line in text.splitlines(keepends=True):
        fence_line = bool(FENCE.match(line))
        if fence_line or in_fence:
            newline = "\n" if line.endswith("\n") else ""
            out.append(" " * len(line.rstrip("\n")) + newline)
        else:
            out.append(line)
        if fence_line:
            in_fence = not in_fence
    return "".join(out)


def slugify(heading_text: str) -> str:
    """Approximate GitHub's heading -> anchor slug algorithm.

    Lowercase, drop characters that are not word/space/hyphen, then turn spaces
    into hyphens. Runs of hyphens are preserved (GitHub does not collapse them),
    so em-dash separators round-trip.
    """
    text = heading_text.strip().lower()
    text = re.sub(r"[^\w\s-]", "", text)
    return text.replace(" ", "-")


def anchors_for(path: Path) -> set[str] | None:
    """Return the anchors a Markdown file exposes, or None if unreadable.

    Both the GitHub auto-slug of each heading and any explicit ``{#custom-id}``
    attributes count. Headings inside fenced code blocks are ignored.
    """
    key = str(Path(path).resolve())
    if key in _anchor_cache:
        return _anchor_cache[key]
    try:
        text = Path(path).read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        _anchor_cache[key] = None
        return None

    anchors: set[str] = set()
    in_fence = False
    for line in text.splitlines():
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        match = HEADING.match(line)
        if not match:
            continue
        raw = match.group(1)
        for explicit in EXPLICIT_ID.findall(raw):
            anchors.add(explicit.strip())
        without_ids = EXPLICIT_ID.sub("", raw).rstrip("#").strip()
        anchors.add(slugify(without_ids))
    _anchor_cache[key] = anchors
    return anchors


def resolve_link(source: Path, target: str) -> str | None:
    """Check a single relative link's file part and optional anchor.

    ``source`` is the file the link lives in; ``target`` is the link body, e.g.
    ``other.md#section`` or ``#same-file-section``. Returns an error string, or
    None when the link resolves. Fragments are only checked against local
    Markdown files we can read.
    """
    file_part, _, fragment = target.partition("#")
    if file_part:
        resolved = (Path(source).parent / file_part).resolve()
        if not resolved.exists():
            return f"broken link -> {target}"
    else:
        resolved = Path(source)

    if not fragment or resolved.suffix != ".md":
        return None
    anchors = anchors_for(resolved)
    if anchors is None:
        return None
    if fragment not in anchors:
        return f"broken anchor -> {target}"
    return None
