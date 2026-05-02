export function isToolAllowed(
  allowedTools: string[] | undefined,
  toolId: string,
): boolean {
  return allowedTools ? allowedTools.includes(toolId) : true;
}

export function nextAllowedTools(
  allowedTools: string[] | undefined,
  allToolIds: string[],
  toolId: string,
  checked: boolean,
): string[] | undefined {
  if (checked) {
    if (!allowedTools) {
      return undefined;
    }

    const nextAllowed = Array.from(new Set([...allowedTools, toolId])).filter((id) =>
      allToolIds.includes(id),
    );
    return nextAllowed.length >= allToolIds.length ? undefined : nextAllowed;
  }

  const baseAllowed = allowedTools ?? allToolIds;
  return baseAllowed.filter((id) => id !== toolId);
}

/// Inclusion semantics — `undefined` (or missing) means "every tool"; an
/// explicit array means "only these tools".
export type ToolSelectionMode = "all" | "custom";

export function toolSelectionMode(
  allowedTools: string[] | undefined,
): ToolSelectionMode {
  return allowedTools === undefined ? "all" : "custom";
}

/// Switch between modes without losing user data. When toggling to "all",
/// the explicit list is cleared (undefined). When toggling to "custom",
/// the current list is preserved, falling back to *all* known tools so
/// the resulting custom set has the same effective behaviour as "all".
export function applyToolSelectionMode(
  current: string[] | undefined,
  mode: ToolSelectionMode,
  allToolIds: string[],
): string[] | undefined {
  if (mode === "all") return undefined;
  if (current !== undefined) return current;
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

/// Apply a "select all in group" operation. Returns the new allowed_tools
/// value, preserving the inclusion semantics: when every tool ends up
/// selected we collapse to `undefined`.
export function setGroupSelection(
  allowedTools: string[] | undefined,
  allToolIds: string[],
  groupToolIds: string[],
  selected: boolean,
): string[] | undefined {
  const baseline = allowedTools ?? allToolIds;
  const baselineSet = new Set(baseline);

  if (selected) {
    for (const id of groupToolIds) {
      baselineSet.add(id);
    }
  } else {
    for (const id of groupToolIds) {
      baselineSet.delete(id);
    }
  }

  if (selected && allToolIds.every((id) => baselineSet.has(id))) {
    return undefined;
  }

  return Array.from(baselineSet).filter((id) => allToolIds.includes(id));
}

/// Returns the selection state of a group: "all" if every tool in the
/// group is selected, "none" if zero, or "some" otherwise.
export function groupSelectionState(
  allowedTools: string[] | undefined,
  groupToolIds: string[],
): "all" | "some" | "none" {
  if (groupToolIds.length === 0) return "none";
  let selected = 0;
  for (const id of groupToolIds) {
    if (isToolAllowed(allowedTools, id)) selected += 1;
  }
  if (selected === 0) return "none";
  if (selected === groupToolIds.length) return "all";
  return "some";
}
