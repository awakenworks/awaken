#!/usr/bin/env python3
"""One module per side may spell the `mcp__<server>__<tool>` identity grammar.

Split out of check_crate_boundaries.py, which reached its hard line limit when
this rule grew a TypeScript side: a console component had re-implemented the
prefix parse the Rust and TS owners already own. The rule is one
responsibility -- who is allowed to spell an identity -- so it is one module,
and check_crate_boundaries imports it.
"""

from __future__ import annotations

import re
import tempfile
from pathlib import Path

import _crate_boundary_workspace

REPO_ROOT = Path(__file__).resolve().parents[2]
CRATES = REPO_ROOT / "crates"




# `mcp__<server>__<tool>` is one identity with one spelling. Its sole
# constructor and sole decomposer live in the module below. Anywhere else, a
# `format!` that builds the grammar or a `strip_prefix` that takes it apart is a
# second translation of the same identity — and a second translation is exactly
# how a sanitizing producer came to be compared against a raw configured server
# name, so an operator's *disabled* MCP toolset silently governed nothing. Three
# normalization layers were added on top of that bug before it was found; the
# fix is to make the second spelling unwritable, not to add a fourth.
#
# Whole-repo scan on purpose: a file list only stops the known offenders from
# regressing, while a scan governs code that does not exist yet. A file that
# genuinely must own another spelling states why here.
# A file recorded here carries a second spelling on purpose; the entry
# must say why, because an unexplained exemption is how a rule stops
# meaning what it says.
MCP_TOOL_IDENTITY_EXEMPTIONS: dict[str, str] = {}

MCP_TOOL_IDENTITY_TS_EXEMPTIONS: dict[str, str] = {}

MCP_TOOL_IDENTITY_OWNER = Path("crates/sdk/contracts/awaken-agent-contract/src/mcp_tool_id.rs")
MCP_TOOL_IDENTITY_GRAMMAR = {
    'format!("mcp__': "build it with awaken_agent_contract::McpToolId::new (or ::namespace)",
    'format!("mcp.': "build it with awaken_agent_contract::McpToolId::dotted",
    'strip_prefix("mcp__"': "take it apart with awaken_agent_contract::McpToolId::parse",
    # Naming the prefix locally is how a second decomposer evades the three
    # fragments above: bind it to a `const` and `strip_prefix` that instead.
    # Reference awaken_agent_contract::MCP_TOOL_ID_PREFIX / MCP_DOTTED_TOOL_PREFIX.
    '"mcp__"': "reference awaken_agent_contract::MCP_TOOL_ID_PREFIX",
    '"mcp."': "reference awaken_agent_contract::MCP_DOTTED_TOOL_PREFIX",
}

# The identity crosses the process boundary, so the rule has to as well. The
# console rewrites authored tool references when an MCP server is renamed, and
# those references are the *encoded* ids the runtime will see; a second console
# spelling diverges from the first exactly the way the Rust duplicate did.
# `web/src/lib/mcp-tool-id.ts` is the console's owner and states the encoding it
# mirrors. A rule scanning only `crates/` was a guard narrower than its subject:
# `ToolOverridesEditor.tsx` re-implemented the prefix parse in plain sight.
MCP_TOOL_IDENTITY_TS_OWNER = Path("web/src/lib/mcp-tool-id.ts")
MCP_TOOL_IDENTITY_TS_ROOT = "web/src"
# Operations on the grammar, not mentions of it: a placeholder string or a
# translated message names the id without translating it, and a name cannot
# diverge from the encoder the way a second parse can.
MCP_TOOL_IDENTITY_TS_GRAMMAR = {
    "`mcp__${": "build it with mcpToolId / mcpToolNamespace from lib/mcp-tool-id",
    "`mcp.${": "build it with the dotted encoder in lib/mcp-tool-id",
    'startsWith("mcp__"': "ask lib/mcp-tool-id (hasMcpToolNamespace), which owns the prefix",
    "startsWith('mcp__'": "ask lib/mcp-tool-id (hasMcpToolNamespace), which owns the prefix",
    '"mcp__".length': "ask lib/mcp-tool-id, which owns the prefix and its length",
    '"mcp__" +': "build it with mcpToolId / mcpToolNamespace from lib/mcp-tool-id",
    "/^mcp__/": "ask lib/mcp-tool-id (hasMcpToolNamespace / isCanonicalMcpToolId)",
    'split("mcp__"': "take it apart with isCanonicalMcpToolId from lib/mcp-tool-id",
}
TS_COMMENT_LINE = re.compile(r"^\s*(?://|\*|/\*)")


def _typescript_without_comments(source: str) -> str:
    """`source` with whole-line comments dropped: prose is not a second spelling."""

    return "\n".join(
        line for line in source.splitlines() if not TS_COMMENT_LINE.match(line)
    )


def check_mcp_tool_identity_is_owned_in_console(repo_root: Path = REPO_ROOT) -> list[str]:
    """The console side of the same rule, over `web/src/**/*.{ts,tsx}`.

    Test modules are separate files here rather than `#[cfg(test)]` items, so
    `*.test.ts(x)` plays the part `production_rust` plays on the Rust side: a
    test may name the encoding, because the shared vectors are how the two sides
    are kept honest.
    """

    errors: list[str] = []
    root = repo_root / MCP_TOOL_IDENTITY_TS_ROOT
    if not root.is_dir():
        return errors
    for path in sorted(root.rglob("*")):
        if path.suffix not in {".ts", ".tsx"} or "node_modules" in path.parts:
            continue
        relative = path.relative_to(repo_root)
        if relative == MCP_TOOL_IDENTITY_TS_OWNER or ".test." in path.name:
            continue
        source = _typescript_without_comments(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for fragment, remedy in MCP_TOOL_IDENTITY_TS_GRAMMAR.items():
            if fragment not in source:
                continue
            if str(relative) in MCP_TOOL_IDENTITY_TS_EXEMPTIONS:
                continue
            errors.append(
                f"{relative}: {fragment!r} re-spells the MCP tool identity grammar "
                f"owned by {MCP_TOOL_IDENTITY_TS_OWNER}; {remedy}, or record in "
                "MCP_TOOL_IDENTITY_TS_EXEMPTIONS why this file must carry a second "
                "spelling of the same identity"
            )
    return errors


def check_mcp_tool_identity_is_owned(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """Only one module *per side* may spell the `mcp__<server>__<tool>` grammar."""
    errors: list[str] = check_mcp_tool_identity_is_owned_in_console(repo_root)
    for path in _crate_boundary_workspace.crate_source_files(crates):
        relative = path.relative_to(repo_root)
        if relative == MCP_TOOL_IDENTITY_OWNER:
            continue
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for fragment, remedy in MCP_TOOL_IDENTITY_GRAMMAR.items():
            if fragment not in production:
                continue
            if str(relative) in MCP_TOOL_IDENTITY_EXEMPTIONS:
                continue
            errors.append(
                f"{relative}: {fragment!r} re-spells the MCP tool identity grammar "
                f"owned by {MCP_TOOL_IDENTITY_OWNER}; {remedy}, or record in "
                "MCP_TOOL_IDENTITY_EXEMPTIONS why this file must carry a second "
                "spelling of the same identity"
            )
    return errors


def selftest_mcp_tool_identity_ownership() -> None:
    """Cause/effect: a second spelling rejects; the owner and a stated one admit."""
    # C1 a file outside the owner builds the id with `format!("mcp__`;
    # C2 it builds the dotted form with `format!("mcp.`; C3 it takes the id
    # apart with `strip_prefix("mcp__"`; C4 the same fragment is in the owning
    # module; C5 it is inside a `#[cfg(test)]` module; C6 it is only in a
    # comment; C7 the file carries a recorded exemption; C8 the file uses the
    # owning type instead. Effects: E1 reject; E2 admit.
    #
    # | Rule | fragment | in owner | test-only | comment | exempt | Effect |
    # | P1   | ctor     | F        | F         | F       | F      | E1     |
    # | P2   | dotted   | F        | F         | F       | F      | E1     |
    # | P3   | strip    | F        | F         | F       | F      | E1     |
    # | P4   | ctor     | T        | F         | F       | F      | E2     |
    # | P5   | ctor     | F        | T         | F       | F      | E2     |
    # | P6   | ctor     | F        | F         | T       | F      | E2     |
    # | P7   | ctor     | F        | F         | F       | T      | E2     |
    # | P8   | none     | F        | F         | F       | F      | E2     |
    # | P9   | const    | F        | F         | F       | F      | E1     |
    #
    # The probes call the same helper the real scan calls, over the same
    # production-Rust projection, so neither a commented copy nor a test-module
    # copy can stand in for the check, and neither can satisfy it.
    constructor = 'fn id(s: &str, t: &str) -> String { format!("mcp__{s}__{t}") }\n'
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        source = crates / "runtime/example/src/lib.rs"
        source.parent.mkdir(parents=True)
        _crate_boundary_workspace.ensure_fixture_manifest(source)

        source.write_text(constructor, encoding="utf-8")
        assert check_mcp_tool_identity_is_owned(root, crates)  # P1
        source.write_text(
            'fn dotted(s: &str, t: &str) -> String { format!("mcp.{s}.{t}") }\n',
            encoding="utf-8",
        )
        assert check_mcp_tool_identity_is_owned(root, crates)  # P2
        source.write_text(
            'fn server(id: &str) -> Option<&str> { id.strip_prefix("mcp__") }\n',
            encoding="utf-8",
        )
        assert check_mcp_tool_identity_is_owned(root, crates)  # P3

        source.unlink()
        owner = root / MCP_TOOL_IDENTITY_OWNER
        owner.parent.mkdir(parents=True)
        owner.write_text(constructor, encoding="utf-8")
        assert not check_mcp_tool_identity_is_owned(root, crates)  # P4

        source.write_text(
            '#[cfg(test)] mod t { fn id() -> String { format!("mcp__a__b") } }\n',
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned(root, crates)  # P5
        source.write_text(
            '// the legacy shape was format!("mcp__{s}__{t}")\nfn id() {}\n',
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned(root, crates)  # P6

        source.write_text(constructor, encoding="utf-8")
        MCP_TOOL_IDENTITY_EXEMPTIONS[str(source.relative_to(root))] = "probe"
        try:
            assert not check_mcp_tool_identity_is_owned(root, crates)  # P7
        finally:
            del MCP_TOOL_IDENTITY_EXEMPTIONS[str(source.relative_to(root))]

        source.write_text(
            "fn id(s: &str, t: &str) -> Option<String> {\n"
            "    awaken_agent_contract::McpToolId::new(s, t)\n"
            "        .ok()\n"
            "        .map(|id| id.as_str().to_string())\n"
            "}\n",
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned(root, crates)  # P8

        source.write_text(
            'const P: &str = "mcp__";\n'
            "fn server(id: &str) -> Option<&str> { id.strip_prefix(P) }\n",
            encoding="utf-8",
        )
        assert check_mcp_tool_identity_is_owned(root, crates)  # P9


def selftest_mcp_tool_identity_ownership_in_console() -> None:
    """The console side of the same table, over every shape `web/src` holds.

    Causes: T1 a component builds the id from a template literal; T2 it takes
    the prefix apart with `startsWith`; T3 it measures the prefix with
    `"mcp__".length`; T4 it matches `/^mcp__/`; T5 it splits on the separator;
    T6 the fragment is in the owning module; T7 it is in a `*.test.ts` vector
    file; T8 it is in a whole-line comment; T9 it is a placeholder or a
    translated message that merely *names* an id; T10 the file carries a
    recorded exemption; T11 it asks the owner instead. Effects: E1 reject;
    E2 admit.

    | Rule | shape            | in owner | test file | comment | exempt | Effect |
    | T1   | `mcp__${`        | F        | F         | F       | F      | E1     |
    | T2   | startsWith       | F        | F         | F       | F      | E1     |
    | T3   | "mcp__".length   | F        | F         | F       | F      | E1     |
    | T4   | /^mcp__/         | F        | F         | F       | F      | E1     |
    | T5   | split("mcp__"    | F        | F         | F       | F      | E1     |
    | T6   | `mcp__${`        | T        | F         | F       | F      | E2     |
    | T7   | `mcp__${`        | F        | T         | F       | F      | E2     |
    | T8   | startsWith       | F        | F         | T       | F      | E2     |
    | T9   | a bare id string | F        | F         | F       | F      | E2     |
    | T10  | startsWith       | F        | F         | F       | T      | E2     |
    | T11  | owner call       | F        | F         | F       | F      | E2     |
    """

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        component = root / "web/src/components/agent/Probe.tsx"
        component.parent.mkdir(parents=True)

        for fragment in (
            'const id = `mcp__${server}__${tool}`;\n',                        # T1
            'const owned = target.startsWith("mcp__");\n',                    # T2
            'const rest = target.slice("mcp__".length);\n',                   # T3
            'const owned = /^mcp__/.test(target);\n',                         # T4
            'const parts = target.split("mcp__")[1];\n',                      # T5
        ):
            component.write_text(fragment, encoding="utf-8")
            assert check_mcp_tool_identity_is_owned_in_console(root), fragment

        component.unlink()
        owner = root / MCP_TOOL_IDENTITY_TS_OWNER
        owner.parent.mkdir(parents=True)
        owner.write_text("const id = `mcp__${server}__${tool}`;\n", encoding="utf-8")
        assert not check_mcp_tool_identity_is_owned_in_console(root)  # T6

        vectors = root / "web/src/lib/mcp-tool-id.test.ts"
        vectors.write_text(
            'expect(mcpToolId("a", "b")).toBe(`mcp__${"a"}__${"b"}`);\n', encoding="utf-8"
        )
        assert not check_mcp_tool_identity_is_owned_in_console(root)  # T7

        component.write_text(
            '// the legacy shape was target.startsWith("mcp__")\n'
            "export const Probe = () => null;\n",
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned_in_console(root)  # T8

        component.write_text(
            'add("mcp__server__tool");\n'
            't("Use mcp__server__tool so the runtime can match the discovered tool.");\n',
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned_in_console(root)  # T9

        component.write_text('const owned = target.startsWith("mcp__");\n', encoding="utf-8")
        MCP_TOOL_IDENTITY_TS_EXEMPTIONS[str(component.relative_to(root))] = "probe"
        try:
            assert not check_mcp_tool_identity_is_owned_in_console(root)  # T10
        finally:
            del MCP_TOOL_IDENTITY_TS_EXEMPTIONS[str(component.relative_to(root))]

        component.write_text(
            'import { hasMcpToolNamespace } from "../../lib/mcp-tool-id";\n'
            "const owned = hasMcpToolNamespace(target);\n",
            encoding="utf-8",
        )
        assert not check_mcp_tool_identity_is_owned_in_console(root)  # T11

    # Corpus assertion: the console owner exists and is the only file the rule
    # exempts by identity, so the scan cannot pass by finding nothing.
    assert (REPO_ROOT / MCP_TOOL_IDENTITY_TS_OWNER).is_file(), (
        f"{MCP_TOOL_IDENTITY_TS_OWNER} is the console's identity owner and must exist"
    )
    assert any(
        path.suffix in {".ts", ".tsx"}
        for path in (REPO_ROOT / MCP_TOOL_IDENTITY_TS_ROOT).rglob("*")
    ), f"{MCP_TOOL_IDENTITY_TS_ROOT} holds no TypeScript; the console scan would be vacuous"






