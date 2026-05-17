/// Mirror of Rust's `AgentSpec` catalog fields. All four are optional —
/// `undefined` means "the user didn't set this list" (different from `[]`
/// which means "the user set an empty list"). This file's matchers preserve
/// that distinction so admin previews match what the runtime will actually
/// do.
export interface AgentSpecCatalog {
  allowed_tools?: string[];
  allowed_tool_patterns?: string[];
  excluded_tools?: string[];
  excluded_tool_patterns?: string[];
}

/// Anchored glob matcher for tool ids. Byte-identical to Rust's
/// `awaken_tool_pattern::tool_id_match`:
///
/// - `*` matches any sequence of characters (including `/`, `:`, `_`).
/// - `\` escapes the next character (`\*` is a literal `*`, `\\` is a literal `\`).
/// - Every other character is a literal.
///
/// Cross-language parity is enforced by the shared JSON fixture which
/// runs against both this function and Rust's.
export function patternMatches(pattern: string, toolId: string): boolean {
  const p = pattern;
  const v = toolId;
  let pi = 0;
  let vi = 0;
  let starPi: number | null = null;
  let starVi = 0;

  while (vi < v.length) {
    if (pi < p.length) {
      const c = p.charCodeAt(pi);
      if (c === 0x5c /* \ */ && pi + 1 < p.length) {
        if (p.charCodeAt(pi + 1) === v.charCodeAt(vi)) {
          pi += 2;
          vi += 1;
          continue;
        }
      } else if (c === 0x2a /* * */) {
        starPi = pi;
        starVi = vi;
        pi += 1;
        continue;
      } else if (c === v.charCodeAt(vi)) {
        pi += 1;
        vi += 1;
        continue;
      }
    }
    if (starPi !== null) {
      pi = starPi + 1;
      starVi += 1;
      vi = starVi;
    } else {
      return false;
    }
  }
  while (pi < p.length && p.charCodeAt(pi) === 0x2a) {
    pi += 1;
  }
  return pi === p.length;
}

/// Decide whether a tool id passes the agent's catalog filter.
///
/// Final allow set = (allowed_tools ∪ allowed_tool_patterns matches)
///                 − (excluded_tools ∪ excluded_tool_patterns matches)
///
/// Deny wins. Mirrors Rust's `AgentSpec::tool_allowed`.
///
/// Note: a catalog with neither `allowed_tools` nor `allowed_tool_patterns`
/// blocks all tools — this is the strict-runtime contract. The Rust
/// deserialize layer injects `["*"]` into `allowed_tool_patterns` for
/// legacy configs missing both fields; the admin console sees post-shim
/// specs so this code does not duplicate that injection.
export function isToolAllowed(catalog: AgentSpecCatalog, toolId: string): boolean {
  const literalAllow = catalog.allowed_tools?.includes(toolId) ?? false;
  const patternAllow =
    catalog.allowed_tool_patterns?.some((p) => patternMatches(p, toolId)) ?? false;
  if (!literalAllow && !patternAllow) return false;
  const literalDeny = catalog.excluded_tools?.includes(toolId) ?? false;
  const patternDeny =
    catalog.excluded_tool_patterns?.some((p) => patternMatches(p, toolId)) ?? false;
  return !(literalDeny || patternDeny);
}

/// For a single pattern entry, return the list of registered tool ids it
/// matches. Useful for inline "matches X, Y, Z" badges next to each pattern.
export function patternMatchesIn(pattern: string, registered: readonly string[]): string[] {
  return registered.filter((id) => patternMatches(pattern, id));
}

/// Allowed-badge label aligned to matcher semantics: "all" iff
/// `allowed_tool_patterns` carries the universal `"*"` glob. Absence of
/// both fields is NOT "all" — that's deny-all in the matcher.
export const deriveAllowedMode = (c: {
  allowed_tool_patterns?: readonly string[] | null;
}): "all" | "custom" => (c.allowed_tool_patterns?.includes("*") ? "all" : "custom");
