/// Variant-aware tool selection helpers.
///
/// `ToolSelector` is used for two AgentSpec fields with opposite semantics:
///
///   - `allowed_tools` (variant `"include"`)
///       null / undefined → every published tool is allowed (default)
///       string[]         → only these tools are allowed
///
///   - `excluded_tools` (variant `"exclude"`)
///       null / undefined → no tool is excluded (default)
///       string[]         → these tools are removed from the allow-list
///
/// The wire-shape (`Option<Vec<String>>`) is identical for both, so we
/// route through a single component, but the *semantics* (what `null`
/// means, what a checkbox "checked" means, what to seed on mode toggle)
/// are inverted. These helpers carry an explicit `variant` so the
/// component never gets the wrong default — silently emitting "all tools
/// excluded" on a Custom-exclusion toggle was R8 #1.
///
/// Mode toggles emit an explicit `null` for "all" (include) / "block
/// none" (exclude) rather than `undefined`. For customized agents this
/// matters: a `null` patch overrides the base record's restricted list
/// with an explicit "no whitelist" / "no blacklist" override; a
/// `undefined` value would land in the save-path's clear list and
/// DELETE the override, re-inheriting the base's restriction — the
/// opposite of what the user picked.

export type ToolSelectionVariant = "include" | "exclude";

/// Whether a tool's checkbox should render checked.
///
/// - include: checked = "tool is in the allow-list". Defaults to true on
///   null/undefined (no whitelist means every tool is allowed).
/// - exclude: checked = "tool is in the exclude-list". Defaults to false
///   on null/undefined (no blacklist means no tool is excluded).
export function isToolSelected(
  value: string[] | null | undefined,
  toolId: string,
  variant: ToolSelectionVariant = "include",
): boolean {
  if (variant === "exclude") {
    return value ? value.includes(toolId) : false;
  }
  return value ? value.includes(toolId) : true;
}

/// Backward-compat alias — older include-only call sites.
export const isToolAllowed = isToolSelected;

/// Compute the next value after a checkbox toggle.
///
/// For include: `checked` = "add to allow-list".
/// For exclude: `checked` = "add to exclude-list".
///
/// Returns explicit `null` when the resulting set means "default
/// everywhere" (every tool allowed / nothing excluded) so customized
/// agents persist the user's intent as an override rather than
/// inheriting the base record.
export function nextToolSelection(
  value: string[] | null | undefined,
  allToolIds: string[],
  toolId: string,
  checked: boolean,
  variant: ToolSelectionVariant = "include",
): string[] | null {
  if (variant === "exclude") {
    const current = value ?? [];
    if (checked) {
      const next = Array.from(new Set([...current, toolId])).filter((id) =>
        allToolIds.includes(id),
      );
      return next.length === 0 ? null : next;
    }
    if (current.length === 0) return null;
    const next = current.filter((id) => id !== toolId);
    return next.length === 0 ? null : next;
  }
  // include
  if (checked) {
    if (!value) {
      // Already "all tools allowed"; checking an already-checked tool is
      // a no-op. Persist as explicit null so customized save emits an
      // override rather than DELETE-ing it (which would inherit the
      // base's restricted list).
      return null;
    }
    const next = Array.from(new Set([...value, toolId])).filter((id) =>
      allToolIds.includes(id),
    );
    return next.length >= allToolIds.length ? null : next;
  }
  const baseAllowed = value ?? allToolIds;
  return baseAllowed.filter((id) => id !== toolId);
}

/// Backward-compat alias.
export const nextAllowedTools = nextToolSelection;

export type ToolSelectionMode = "all" | "custom";

export function toolSelectionMode(
  value: string[] | null | undefined,
): ToolSelectionMode {
  return value == null ? "all" : "custom";
}

/// Switch between modes without losing user data.
///
/// "all" → explicit `null` (forces "all tools" / "block none" as an
/// override; does NOT mean "inherit base").
///
/// "custom" with no prior list:
///   - include: seeded with every known tool (so checkboxes start
///     checked and the user deselects to narrow the list).
///   - exclude: seeded with `[]` (so checkboxes start UNCHECKED — seeding
///     with every tool would mean every tool is excluded, i.e. the agent
///     loses access to everything; this was R8 #1).
export function applyToolSelectionMode(
  current: string[] | null | undefined,
  mode: ToolSelectionMode,
  allToolIds: string[],
  variant: ToolSelectionVariant = "include",
): string[] | null {
  if (mode === "all") return null;
  if (current && current.length > 0) return current;
  if (variant === "exclude") return [];
  return [...allToolIds];
}

/// Source bucket for grouping tools in the UI. We classify by id prefix:
/// `mcp:server-id/...` → an MCP server; `plugin:...` → a plugin tool;
/// otherwise it's a built-in.
export type ToolSourceKind = "mcp" | "plugin" | "builtin";

export interface ToolSource {
  kind: ToolSourceKind;
  /// Display label, e.g. "MCP · weather-service", "Plugin", "Built-in".
  label: string;
  /// Stable group key used for ordering and selection toggles.
  key: string;
}

/// Backend-supplied source descriptor (from `/v1/capabilities`).
export interface ApiToolSource {
  kind: "builtin" | "plugin" | "mcp";
  id?: string;
}

/// Derive a ToolSource from a backend-supplied source descriptor when present,
/// falling back to id-prefix inference for tools without an explicit source.
export function toolSourceFor(
  toolId: string,
  apiSource?: ApiToolSource,
): ToolSource {
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
  // Fallback: infer from id prefix (legacy / tools without explicit source).
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

/// "Select all" / "Clear" buttons on a group.
///
/// For include: selected=true means "allow every tool in this group" —
/// collapses to `null` when every known tool ends up allowed.
///
/// For exclude: selected=true means "exclude every tool in this group" —
/// collapses to `null` when no tool ends up excluded.
export function setGroupSelection(
  value: string[] | null | undefined,
  allToolIds: string[],
  groupToolIds: string[],
  selected: boolean,
  variant: ToolSelectionVariant = "include",
): string[] | null {
  if (variant === "exclude") {
    const baseline = new Set(value ?? []);
    if (selected) {
      for (const id of groupToolIds) baseline.add(id);
    } else {
      for (const id of groupToolIds) baseline.delete(id);
    }
    const next = Array.from(baseline).filter((id) => allToolIds.includes(id));
    return next.length === 0 ? null : next;
  }
  const baseline = new Set(value ?? allToolIds);

  if (selected) {
    for (const id of groupToolIds) baseline.add(id);
  } else {
    for (const id of groupToolIds) baseline.delete(id);
  }

  if (selected && allToolIds.every((id) => baseline.has(id))) {
    return null;
  }

  return Array.from(baseline).filter((id) => allToolIds.includes(id));
}

/// Selection state of a group of tools — used for the per-group "X of N
/// selected" summary and the indeterminate visual state on group
/// buttons. "Selected" follows the variant: included for `"include"`,
/// excluded for `"exclude"`.
export function groupSelectionState(
  value: string[] | null | undefined,
  groupToolIds: string[],
  variant: ToolSelectionVariant = "include",
): "all" | "some" | "none" {
  if (groupToolIds.length === 0) return "none";
  let selected = 0;
  for (const id of groupToolIds) {
    if (isToolSelected(value, id, variant)) selected += 1;
  }
  if (selected === 0) return "none";
  if (selected === groupToolIds.length) return "all";
  return "some";
}
