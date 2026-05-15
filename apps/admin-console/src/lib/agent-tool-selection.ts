/// Catalog semantics for `allowed_tools` / `excluded_tools`.
///
///   ["*"]            - explicit all-pattern (matches every tool)
///   []               - explicit empty (matches nothing)
///   ["a", "b*"]      - subset; entries may be literal ids or tool-id patterns
///   null / undefined - deprecated legacy value; save migrates to explicit form
///
/// The legacy `null`/`undefined` shape is retained for read compatibility
/// only — every helper below normalises it but `isLegacyCatalogValue` lets
/// callers (e.g. the editor UI) surface a deprecation hint.

const EXPLICIT_ALL = "*";

export type CatalogVariant = "include" | "exclude";

function allModeValue(variant: CatalogVariant): string[] {
  return variant === "include" ? [EXPLICIT_ALL] : [];
}

export function isLegacyCatalogValue(value: string[] | null | undefined): boolean {
  return value == null;
}

export function isExplicitAll(value: string[] | null | undefined): boolean {
  return Array.isArray(value) && value.length === 1 && value[0] === EXPLICIT_ALL;
}

export function hasUnescapedCatalogWildcard(entry: string): boolean {
  let escaped = false;
  for (const char of entry) {
    if (escaped) {
      escaped = false;
      continue;
    }
    if (char === "\\") {
      escaped = true;
      continue;
    }
    if (char === EXPLICIT_ALL) return true;
  }
  return false;
}

/// Match a tool-id pattern against a literal tool id. Mirrors the runtime's
/// `awaken_tool_pattern::tool_id_match` — see the shared parity fixture at
/// `crates/awaken-tool-pattern/tests/fixtures/catalog-glob-parity.json`.
///
/// Grammar:
/// - The full pattern must match the full tool id (anchored).
/// - `*` matches any sequence of characters (including `/`, `:`, `_`).
/// - `\` escapes the next character (`\*` = literal `*`; `\\` = literal `\`).
/// - Every other character is a literal — there is no `**`, `?`, `[...]`,
///   `{a,b}`, leading-`!` negation, or regex syntax at this layer.
///
/// Exported for the parity test fixture; prefer `isToolAllowed` in component code.
export function toolIdMatch(pattern: string, value: string): boolean {
  const p = pattern;
  const v = value;
  let pi = 0;
  let vi = 0;
  let starPi: number | null = null;
  let starVi = 0;

  while (vi < v.length) {
    if (pi < p.length) {
      const c = p[pi];
      if (c === "\\" && pi + 1 < p.length) {
        if (p[pi + 1] === v[vi]) {
          pi += 2;
          vi += 1;
          continue;
        }
      } else if (c === "*") {
        starPi = pi;
        starVi = vi;
        pi += 1;
        continue;
      } else if (c === v[vi]) {
        pi += 1;
        vi += 1;
        continue;
      }
    }
    // Mismatch — backtrack to the last `*` and consume one more value char.
    if (starPi !== null) {
      pi = starPi + 1;
      starVi += 1;
      vi = starVi;
    } else {
      return false;
    }
  }
  while (pi < p.length && p[pi] === "*") {
    pi += 1;
  }
  return pi === p.length;
}

function isKnownToolId(entry: string, allToolIds: string[]): boolean {
  return allToolIds.includes(entry);
}

export interface CatalogEntryInspection {
  entry: string;
  exactToolExists: boolean;
  usesWildcard: boolean;
  matches: string[];
}

export function catalogEntryInspections(
  value: string[] | null | undefined,
  allToolIds: string[],
  variant: CatalogVariant = "include",
): CatalogEntryInspection[] {
  if (!Array.isArray(value)) return [];
  if (variant === "include" && isExplicitAll(value)) return [];

  const seen = new Set<string>();
  const entries: CatalogEntryInspection[] = [];
  for (const entry of value) {
    if (seen.has(entry)) continue;
    seen.add(entry);
    const usesWildcard = hasUnescapedCatalogWildcard(entry);
    const exactToolExists = isKnownToolId(entry, allToolIds);
    if (exactToolExists && !usesWildcard) continue;
    entries.push({
      entry,
      exactToolExists,
      usesWildcard,
      matches: allToolIds.filter((toolId) => toolIdMatch(entry, toolId)),
    });
  }
  return entries;
}

export function removeCatalogEntry(
  value: string[] | null | undefined,
  entry: string,
): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((candidate) => candidate !== entry);
}

export function isToolAllowed(
  allowedTools: string[] | null | undefined,
  toolId: string,
  variant: CatalogVariant = "include",
): boolean {
  if (allowedTools == null) return variant === "include";
  if (isExplicitAll(allowedTools)) return true;
  return allowedTools.some((entry) => toolIdMatch(entry, toolId));
}

function expandSubset(
  value: string[] | null | undefined,
  allToolIds: string[],
  variant: CatalogVariant,
): string[] {
  if (value == null) return variant === "include" ? [...allToolIds] : [];
  if (isExplicitAll(value)) return [...allToolIds];
  return [...value];
}

export function isToolSelectionPatternBacked(
  allowedTools: string[] | null | undefined,
  toolId: string,
  variant: CatalogVariant = "include",
): boolean {
  return toolSelectionPattern(allowedTools, toolId, variant) !== null;
}

/// Return the entry from `allowedTools` that pattern-matched `toolId` (so
/// the UI can show which pattern is responsible). Any non-literal entry
/// that grants access counts — including escaped literals such as `\!Bash`
/// that the docs advertise as valid catalog grammar. Returns `null` when
/// the tool's selection comes from an exact literal entry or nothing.
export function toolSelectionPattern(
  allowedTools: string[] | null | undefined,
  toolId: string,
  variant: CatalogVariant = "include",
): string | null {
  if (!Array.isArray(allowedTools)) return null;
  if (isExplicitAll(allowedTools)) return variant === "exclude" ? EXPLICIT_ALL : null;
  for (const entry of allowedTools) {
    if (entry !== toolId && toolIdMatch(entry, toolId)) return entry;
  }
  return null;
}

function hasUnmanagedCatalogEntries(value: string[], allToolIds: string[]): boolean {
  return value.some((entry) => !isKnownToolId(entry, allToolIds));
}

export function nextAllowedTools(
  allowedTools: string[] | null | undefined,
  allToolIds: string[],
  toolId: string,
  checked: boolean,
  variant: CatalogVariant = "include",
): string[] {
  if (variant === "exclude" && isExplicitAll(allowedTools)) return [EXPLICIT_ALL];

  const baseline = expandSubset(allowedTools, allToolIds, variant);
  if (checked) {
    const next = isKnownToolId(toolId, allToolIds)
      ? Array.from(new Set([...baseline, toolId]))
      : baseline;
    if (
      variant === "include" &&
      !hasUnmanagedCatalogEntries(next, allToolIds) &&
      allToolIds.every((id) => next.includes(id))
    ) {
      return [EXPLICIT_ALL];
    }
    return next;
  }
  return baseline.filter((id) => id !== toolId);
}

export type ToolSelectionMode = "all" | "custom";

export function toolSelectionMode(
  allowedTools: string[] | null | undefined,
  variant: CatalogVariant = "include",
): ToolSelectionMode {
  if (allowedTools == null) return "all";
  // For include variant: "all" means "allow every tool" — ["*"].
  // For exclude variant: "all" means "block none" — [].
  if (variant === "include" && isExplicitAll(allowedTools)) return "all";
  if (variant === "exclude" && allowedTools.length === 0) return "all";
  return "custom";
}

export function applyToolSelectionMode(
  current: string[] | null | undefined,
  mode: ToolSelectionMode,
  allToolIds: string[],
  variant: CatalogVariant = "include",
): string[] {
  if (mode === "all") return allModeValue(variant);
  if (current != null && !isExplicitAll(current)) return [...current];
  if (variant === "exclude" && current == null) return [];
  return [...allToolIds];
}

export type ToolSourceKind = "mcp" | "plugin" | "builtin";

export interface ToolSource {
  kind: ToolSourceKind;
  /// Display label, e.g. "MCP · weather-service", "Plugin", "Built-in".
  label: string;
  /// Stable group key used for ordering and selection toggles.
  key: string;
}

export interface ApiToolSource {
  kind: "builtin" | "plugin" | "mcp";
  id?: string;
}

export function toolSourceFor(toolId: string, apiSource?: ApiToolSource): ToolSource {
  if (apiSource) {
    if (apiSource.kind === "mcp") {
      const server = apiSource.id ?? "";
      const label = server.length > 0 ? `MCP · ${server}` : "MCP";
      return { kind: "mcp", label, key: `mcp:${server}` };
    }
    if (apiSource.kind === "plugin") {
      const plugin = apiSource.id ?? "";
      const label = plugin.length > 0 ? `Plugin · ${plugin}` : "Plugin";
      return { kind: "plugin", label, key: `plugin:${plugin}` };
    }
    return { kind: "builtin", label: "Built-in", key: "builtin" };
  }
  if (toolId.startsWith("mcp:")) {
    const remainder = toolId.slice(4);
    const slash = remainder.indexOf("/");
    const server = slash >= 0 ? remainder.slice(0, slash) : remainder;
    const label = server.length > 0 ? `MCP · ${server}` : "MCP";
    return { kind: "mcp", label, key: `mcp:${server}` };
  }
  if (toolId.startsWith("plugin:")) {
    const remainder = toolId.slice(7);
    const slash = remainder.indexOf("/");
    const plugin = slash >= 0 ? remainder.slice(0, slash) : remainder;
    const label = plugin.length > 0 ? `Plugin · ${plugin}` : "Plugin";
    return { kind: "plugin", label, key: `plugin:${plugin}` };
  }
  return { kind: "builtin", label: "Built-in", key: "builtin" };
}

export interface ToolGroup<TTool> {
  source: ToolSource;
  tools: TTool[];
}

export function groupToolsBySource<TTool extends { id: string; source?: ApiToolSource }>(
  tools: TTool[],
): ToolGroup<TTool>[] {
  const buckets = new Map<string, ToolGroup<TTool>>();
  for (const tool of tools) {
    const source = toolSourceFor(tool.id, tool.source);
    let bucket = buckets.get(source.key);
    if (!bucket) {
      bucket = { source, tools: [] };
      buckets.set(source.key, bucket);
    }
    bucket.tools.push(tool);
  }

  for (const bucket of buckets.values()) {
    bucket.tools.sort((a, b) => a.id.localeCompare(b.id));
  }

  return Array.from(buckets.values()).sort((a, b) => {
    if (a.source.kind !== b.source.kind) {
      return SOURCE_ORDER[a.source.kind] - SOURCE_ORDER[b.source.kind];
    }
    return a.source.label.localeCompare(b.source.label);
  });
}

const SOURCE_ORDER: Record<ToolSourceKind, number> = {
  builtin: 0,
  plugin: 1,
  mcp: 2,
};

export function setGroupSelection(
  allowedTools: string[] | null | undefined,
  allToolIds: string[],
  groupToolIds: string[],
  selected: boolean,
  variant: CatalogVariant = "include",
): string[] {
  if (variant === "exclude" && isExplicitAll(allowedTools)) return [EXPLICIT_ALL];

  const baseline = new Set(expandSubset(allowedTools, allToolIds, variant));

  if (selected) {
    for (const id of groupToolIds) {
      if (isKnownToolId(id, allToolIds)) baseline.add(id);
    }
  } else {
    for (const id of groupToolIds) {
      baseline.delete(id);
    }
  }

  const next = Array.from(baseline);
  if (
    selected &&
    variant === "include" &&
    !hasUnmanagedCatalogEntries(next, allToolIds) &&
    allToolIds.every((id) => baseline.has(id))
  ) {
    return [EXPLICIT_ALL];
  }

  return next;
}

export function groupSelectionState(
  allowedTools: string[] | null | undefined,
  groupToolIds: string[],
  variant: CatalogVariant = "include",
): "all" | "some" | "none" {
  if (groupToolIds.length === 0) return "none";
  let selected = 0;
  for (const id of groupToolIds) {
    if (isToolAllowed(allowedTools, id, variant)) selected += 1;
  }
  if (selected === 0) return "none";
  if (selected === groupToolIds.length) return "all";
  return "some";
}
