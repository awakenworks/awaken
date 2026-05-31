import type { AgentSpec } from "./config-api";
import { readSkillAllowlist, selectedMcpServerIds } from "./agent-resource-references";

export type AgentEditorTabId =
  | "basics"
  | "tools"
  | "skills"
  | "plugins"
  | "delegates"
  | "advanced"
  | "history";

export interface AgentEditorTab {
  id: AgentEditorTabId;
  label: string;
  description: string;
  /** Returns a badge string (e.g. "23" or "•") for the tab tab. Empty
   *  string / null means no badge. */
  badge?: (spec: AgentSpec) => string | null;
}

export const AGENT_EDITOR_TABS: readonly AgentEditorTab[] = [
  { id: "basics", label: "Basics", description: "Identity, model, prompt, limits." },
  {
    id: "tools",
    label: "Tools",
    description: "Allowed and excluded tools.",
    badge: (spec) => {
      const allowed = spec.allowed_tools?.length ?? 0;
      const excluded = spec.excluded_tools?.length ?? 0;
      const mcp = selectedMcpServerIds(spec).length;
      if (allowed === 0 && excluded === 0 && mcp === 0) return null;
      const base = excluded > 0 ? `${allowed}·−${excluded}` : String(allowed);
      return mcp > 0 ? `${base}·m${mcp}` : base;
    },
  },
  {
    id: "skills",
    label: "Skills",
    description: "Skill catalog and skill runtime plugins.",
    badge: (spec) => {
      const allowlist = readSkillAllowlist(spec);
      return allowlist ? String(allowlist.length) : null;
    },
  },
  {
    id: "plugins",
    label: "Plugins",
    description: "Enabled plugins and their config.",
    badge: (spec) =>
      spec.plugin_ids && spec.plugin_ids.length > 0
        ? String(spec.plugin_ids.length)
        : null,
  },
  {
    id: "delegates",
    label: "Delegates",
    description: "Agents this one can delegate to.",
    badge: (spec) =>
      spec.delegates && spec.delegates.length > 0
        ? String(spec.delegates.length)
        : null,
  },
  { id: "advanced", label: "Advanced", description: "Reasoning, limits, raw JSON." },
  { id: "history", label: "History", description: "Audit history and restore." },
];

export const DEFAULT_AGENT_EDITOR_TAB: AgentEditorTabId = "basics";

export function isAgentEditorTab(value: unknown): value is AgentEditorTabId {
  return (
    typeof value === "string" &&
    AGENT_EDITOR_TABS.some((tab) => tab.id === value)
  );
}

/// Read the active tab from a query-string parameter (typically
/// `?tab=tools`). Falls back to the default when missing or unrecognised.
export function readTabFromSearch(
  search: URLSearchParams | string,
): AgentEditorTabId {
  const params =
    typeof search === "string" ? new URLSearchParams(search) : search;
  const candidate = params.get("tab");
  return isAgentEditorTab(candidate) ? candidate : DEFAULT_AGENT_EDITOR_TAB;
}

/// Produce the next URLSearchParams for a tab change. The default tab is
/// represented by removing the `tab` parameter so the canonical URL stays
/// clean.
export function writeTabToSearch(
  search: URLSearchParams | string,
  tab: AgentEditorTabId,
): URLSearchParams {
  const params = new URLSearchParams(
    typeof search === "string" ? search : search.toString(),
  );
  if (tab === DEFAULT_AGENT_EDITOR_TAB) {
    params.delete("tab");
  } else {
    params.set("tab", tab);
  }
  return params;
}
