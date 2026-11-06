"""One ownership-fence kernel: lease, epoch and slot identities only grow inside it.

The largest product bug class in this repository is an ownership identity that
is compared incompletely: a fence, CAS or lease token that omits its subject,
its holder or its generation, or two drivers holding one resource. The kernel
in `awaken-lease-contract` (`fence` module) owns the complete identity --
`Epoch`, `Tenure`, `Lease`, `EffectFence`, `SlotToken` -- and this gate keeps
new code on it:

F1  No new type named `*Fence`, `*Lease`, `*Tenure` or `*Epoch` outside the
    kernel unless its definition wraps a kernel type. Names that already exist
    at the reference commit are the legacy remainder and may stay.
F2  The count of `*epoch*: u64` declarations outside the kernel only falls.
F3  The count of hand-written epoch increments (`epoch += 1`, `epoch + 1`,
    `epoch.checked_add(1)` ...) outside the kernel only falls: new successors
    go through `Epoch::next`, which refuses to wrap.
F5  `#[serde(default)]` on a non-optional identity field (a name ending in
    `epoch`, `owner`, `incarnation`, `token` or `generation`) is a hard error:
    a missing field then decodes as `0` or `""`, an identity no grant minted.
    An absent identity must be spelled `Option<...>`.
F6  `Arc<()>` outside the kernel is a hard error: an allocation identity is a
    `SlotToken`.

F2 and F3 never read a hand-edited baseline. The ceiling is the same count at
the reference commit: `HEAD` for a local (pre-commit) run, and the merge-base
with the target branch in CI (`CI` set), which fails closed when that history
is unavailable. `AWAKEN_FENCE_FITNESS_BASE` names another reference explicitly.
"""

from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path

# The kernel's own sources: `fence.rs` and the `fence/` module directory.
KERNEL = "crates/sdk/contracts/awaken-lease-contract/src/fence"

FENCE_TYPE = re.compile(
    r"\b(?P<kind>struct|enum|type|trait)\s+(?P<name>\w*(?:Fence|Lease|Tenure|Epoch))\b"
)
KERNEL_TYPE = re.compile(r"\b(?:Lease|Tenure|EffectFence|Epoch|SlotToken)\b")
# A qualified primitive value path is not a raw type annotation.
EPOCH_U64 = re.compile(r"\b\w*epoch\w*\s*:\s*u64\b(?!\s*::)")
EPOCH_INCREMENT = re.compile(
    r"\b\w*epoch\w*\s*(?:\+=\s*1\b|\+\s*1\b|\.\s*(?:checked|saturating|wrapping)_add\s*\(\s*1\s*\))"
)
ARC_UNIT = re.compile(r"\bArc\s*<\s*\(\s*\)\s*>")
SERDE_DEFAULT_FIELD = re.compile(
    r"#\[serde\((?P<args>[^\]]*)\)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([a-z:]+\))?\s+)?(?P<name>[a-z_][a-z0-9_]*)\s*:\s*(?P<type>[^,\n]+)"
)
IDENTITY_FIELD = re.compile(r"(?:^|_)(?:epoch|owner|incarnation|token|generation)$")
OPTIONAL_TYPE = re.compile(r"^(?:Option|Vec|BTreeMap|HashMap|BTreeSet|HashSet)\s*<")


class ReferenceUnavailable(Exception):
    """The reference commit the ratchets compare against cannot be read."""


def _in_kernel(path: str) -> bool:
    return path == f"{KERNEL}.rs" or path.startswith(f"{KERNEL}/")


# A file that mentions none of these cannot contribute to F1-F3 or F6, so only
# the files that do are projected (the projection is the expensive step).
RELEVANT = re.compile(r"epoch|Epoch|Fence|Lease|Tenure|Arc\s*<")


# Comments, string/char literals and raw strings, in one pass. Blanking them
# keeps line numbers; a char literal is matched before a string so `'"'` does
# not open one, and a lifetime (`'a` with no closing quote) is never a literal.
NON_CODE = re.compile(
    r"'(?:\\.|[^'\\\n])'"
    r"|//[^\n]*"
    r"|/\*.*?\*/"
    r'|r(#*)".*?"\1'
    r'|"(?:\\.|[^"\\])*"',
    re.S,
)


def _blank(match: re.Match[str]) -> str:
    text = match.group(0)
    return "\n".join(" " * len(line) for line in text.split("\n"))


def _code(sources: dict[str, str]) -> dict[str, str]:
    """Comments and literal contents blanked, so prose never counts; kernel
    files and files that cannot match any rule are left out."""

    return {
        path: NON_CODE.sub(_blank, source)
        for path, source in sources.items()
        if not _in_kernel(path) and RELEVANT.search(source)
    }


def _item_end(code: str, start: int) -> int:
    """End of the item whose declaration starts at `start`: `;` or its block."""

    depth = 0
    for index in range(start, len(code)):
        character = code[index]
        if character in "{(":
            depth += 1
        elif character in "})":
            depth -= 1
            if depth == 0 and character == "}":
                return index + 1
        elif character == ";" and depth == 0:
            return index + 1
    return len(code)


def fence_types(projected: dict[str, str]) -> dict[str, list[tuple[str, bool]]]:
    """Every fence-named declaration outside the kernel: name -> (path, wraps)."""

    found: dict[str, list[tuple[str, bool]]] = {}
    for path, code in projected.items():
        for match in FENCE_TYPE.finditer(code):
            body = code[match.end() : _item_end(code, match.end())]
            wraps = match.group("kind") != "trait" and bool(KERNEL_TYPE.search(body))
            found.setdefault(match.group("name"), []).append((path, wraps))
    return found


def _count(projected: dict[str, str], pattern: re.Pattern[str]) -> int:
    return sum(len(pattern.findall(code)) for code in projected.values())


def identity_defaults(sources: dict[str, str]) -> list[str]:
    """`#[serde(default)]` on a non-optional identity field (F5)."""

    found: list[str] = []
    for path, source in sources.items():
        if "default" not in source or "serde" not in source:
            continue
        code = NON_CODE.sub(
            lambda match: match.group(0) if match.group(0).startswith('"') else _blank(match),
            source,
        )
        for match in SERDE_DEFAULT_FIELD.finditer(code):
            if not re.search(r"\bdefault\b", match.group("args")):
                continue
            if not IDENTITY_FIELD.search(match.group("name")):
                continue
            if OPTIONAL_TYPE.match(match.group("type").strip()):
                continue
            line = code.count("\n", 0, match.start()) + 1
            found.append(f"{path}:{line}: `{match.group('name')}`")
    return found


def unit_arcs(projected: dict[str, str]) -> list[str]:
    """`Arc<()>` allocation identities outside the kernel (F6)."""

    found: list[str] = []
    for path, code in projected.items():
        for match in ARC_UNIT.finditer(code):
            found.append(f"{path}:{code.count(chr(10), 0, match.start()) + 1}")
    return found


def ownership_fence_violations(
    reference: dict[str, str], current: dict[str, str]
) -> list[str]:
    errors: list[str] = []
    reference, projected = _code(reference), _code(current)
    legacy = set(fence_types(reference))
    for name, declarations in sorted(fence_types(projected).items()):
        if name in legacy:
            continue
        for path, wraps in declarations:
            if not wraps:
                errors.append(
                    f"F1 {path}: new type `{name}` names a fence, lease, tenure or "
                    "epoch outside the kernel without wrapping a kernel type "
                    "(awaken_lease_contract::fence::{Lease, Tenure, EffectFence, "
                    "Epoch, SlotToken}); compose the kernel instead of restating it"
                )
    for rule, pattern, what in (
        ("F2", EPOCH_U64, "`*epoch*: u64` declarations"),
        ("F3", EPOCH_INCREMENT, "hand-written epoch increments"),
    ):
        ceiling, now = _count(reference, pattern), _count(projected, pattern)
        if now > ceiling:
            errors.append(
                f"{rule} {what} rose from {ceiling} to {now} outside the kernel; "
                "use awaken_lease_contract::fence::Epoch (and Epoch::next) instead"
            )
    for location in identity_defaults(current):
        errors.append(
            f"F5 {location} has #[serde(default)] on a non-optional identity field; "
            "a missing field would decode as an identity no grant minted -- "
            "require it, or spell the absent case as Option<...>"
        )
    for location in unit_arcs(projected):
        errors.append(
            f"F6 {location}: Arc<()> is an allocation identity; use "
            "awaken_lease_contract::fence::SlotToken"
        )
    return errors


def _git(repo_root: Path, *args: str, stdin: bytes | None = None) -> bytes:
    completed = subprocess.run(
        ["git", *args],
        cwd=repo_root,
        input=stdin,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        raise ReferenceUnavailable(
            f"git {' '.join(args)} failed: {completed.stderr.decode(errors='replace').strip()}"
        )
    return completed.stdout


def reference_commit(repo_root: Path) -> str:
    """HEAD locally; the merge-base with the target branch in CI."""

    explicit = os.environ.get("AWAKEN_FENCE_FITNESS_BASE")
    if explicit:
        target = explicit
    elif os.environ.get("CI"):
        branch = os.environ.get("GITHUB_BASE_REF") or "main"
        target = f"origin/{branch}"
    else:
        return _git(repo_root, "rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    return _git(repo_root, "merge-base", target, "HEAD").decode().strip()


def reference_sources(repo_root: Path, commit: str) -> dict[str, str]:
    """Every crate Rust source at `commit`, read in one `cat-file` batch."""

    names = [
        name
        for name in _git(repo_root, "ls-tree", "-r", "--name-only", commit, "--", "crates")
        .decode()
        .splitlines()
        if name.endswith(".rs")
    ]
    request = "".join(f"{commit}:{name}\n" for name in names).encode()
    batch = _git(repo_root, "cat-file", "--batch", stdin=request)
    sources: dict[str, str] = {}
    cursor = 0
    for name in names:
        header_end = batch.index(b"\n", cursor)
        header = batch[cursor:header_end].split()
        if len(header) != 3 or header[1] != b"blob":
            raise ReferenceUnavailable(f"{commit}:{name} is not a readable blob")
        size = int(header[2])
        body_start = header_end + 1
        sources[name] = batch[body_start : body_start + size].decode("utf-8", errors="replace")
        cursor = body_start + size + 1
    return sources


def check_all(repo_root: Path) -> list[str]:
    current = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in (repo_root / "crates").rglob("*.rs")
        if "target" not in path.relative_to(repo_root).parts
    }
    try:
        reference = reference_sources(repo_root, reference_commit(repo_root))
    except (ReferenceUnavailable, OSError) as error:
        return [
            "ownership-fence ratchet cannot read its reference commit, so it "
            f"refuses to pass: {error}"
        ]
    return ownership_fence_violations(reference, current)


def selftest() -> None:
    kernel = f"{KERNEL}/tenure.rs"
    base = {
        "crates/a/src/lib.rs": (
            "pub struct OldLease { pub epoch: u64 }\n"
            "fn bump(l: &mut OldLease) { l.epoch += 1; }\n"
        ),
        kernel: "pub struct Lease<S>(S);\n",
    }

    def violations(changes: dict[str, str]) -> list[str]:
        return ownership_fence_violations(base, {**base, **changes})

    # | Rule | change                                              | effect   |
    # | F1a  | new `struct NewLease(u64)` outside the kernel        | refused  |
    # | F1b  | new `struct WrapLease(Lease<X>)`                      | admitted |
    # | F1c  | new `trait NewFence`                                  | refused  |
    # | F1d  | legacy `OldLease` kept; new type inside the kernel    | admitted |
    # | F2   | one more `*epoch*: u64` declaration                   | refused  |
    # | F2'  | one fewer                                             | admitted |
    # | F3   | one more hand-written `epoch + 1`                     | refused  |
    # | F5a  | `#[serde(default)] pub epoch: u64`                    | refused  |
    # | F5b  | `#[serde(default)] pub epoch: Option<Epoch>`          | admitted |
    # | F5c  | `#[serde(default)] writer_token: String`              | refused  |
    # | F6a  | `Arc<()>` outside the kernel                          | refused  |
    # | F6b  | `Arc<()>` inside the kernel                           | admitted |
    # | C1   | fence words only in comments and strings              | admitted |
    assert any("F1" in e for e in violations({"crates/b.rs": "struct NewLease(u64);"})), "F1a"
    assert not violations({"crates/b.rs": "struct WrapLease(Lease<X>);"}), "F1b"
    assert any("F1" in e for e in violations({"crates/b.rs": "trait NewFence {}"})), "F1c"
    assert not violations({f"{KERNEL}/extra.rs": "pub struct SlotLease(u8);"}), "F1d"
    assert any(
        "F2" in e for e in violations({"crates/b.rs": "struct A { next_epoch: u64 }"})
    ), "F2"
    assert not violations(
        {"crates/a/src/lib.rs": "pub struct OldLease;\nfn bump(l: &mut u8) {}\n"}
    ), "F2'"
    # F2 cause/effect table: C1 an epoch name has a primitive type annotation;
    # C2 u64 instead starts a qualified value path; C3 that value occurs in
    # the reference/current snapshot. F2d: C1 -> one declaration, refused when
    # added. F2v: C2 -> no declaration, regardless of spaces or blanked comments.
    # F2r: removing a reference-side value cannot pay for a new C1 declaration.
    # Existing C1/kernel rows below retain the non-code and kernel exclusions.
    for declaration in (
        "struct A { next_epoch: u64 }",
        "fn typed(epoch: u64) {}",
        "fn typed() { let epoch: u64 = 0; }",
        "fn typed() { let epoch: u64 = u64::MAX; }",
        "struct A { next_epoch: u64 /* type annotation */, }",
    ):
        assert any(
            "F2" in error for error in violations({"crates/b.rs": declaration})
        ), f"F2d {declaration}"
    for value in (
        "u64::MAX",
        "u64::try_from(3).unwrap()",
        "u64 :: MAX",
        "u64 /* value path */ :: MAX",
        "u64 // value path\n ::try_from(3).unwrap()",
        "u64\n::MAX",
    ):
        assert not violations(
            {"crates/b.rs": f"fn value() {{ let _ = OldLease {{ epoch: {value} }}; }}"}
        ), f"F2v {value}"
    assert _count(
        _code({"crates/b.rs": "struct A { epoch: u64 } fn f() { let _ = A { epoch: u64::MAX }; let _ = A { epoch: u64::try_from(3).unwrap() }; }"}),
        EPOCH_U64,
    ) == 1, "F2m count the annotation once despite multiple value initializers"
    assert any(
        "F2" in error
        for error in ownership_fence_violations(
            {"crates/b.rs": "struct A { epoch: u64 } fn f() { let _ = A { epoch: u64::MAX }; }"},
            {"crates/b.rs": "struct A { epoch: u64 } fn f(next_epoch: u64) {}"},
        )
    ), "F2r reference value paths cannot offset an added declaration"
    assert any(
        "F3" in e for e in violations({"crates/b.rs": "fn f(e: Epoch) { let n = lease_epoch + 1; }"})
    ), "F3"
    assert any(
        "F5" in e
        for e in violations({"crates/b.rs": "struct S {\n    #[serde(default)]\n    pub epoch: Epoch,\n}"})
    ), "F5a"
    assert not violations(
        {"crates/b.rs": "struct S {\n    #[serde(default)]\n    pub epoch: Option<Epoch>,\n}"}
    ), "F5b"
    assert any(
        "F5" in e
        for e in violations(
            {"crates/b.rs": "struct S {\n    #[serde(default, rename = \"t\")]\n    writer_token: String,\n}"}
        )
    ), "F5c"
    assert any("F6" in e for e in violations({"crates/b.rs": "struct S(Arc<()>);"})), "F6a"
    assert not violations({f"{KERNEL}/slot.rs": "struct SlotToken(Arc<()>);"}), "F6b"
    assert not violations(
        {"crates/b.rs": "// struct NewLease; epoch: u64; epoch += 1; Arc<()>\nconst S: &str = \"epoch: u64\";"}
    ), "C1"
