export type AgentEditorTabId =
  | "basics"
  | "tools"
  | "plugins"
  | "delegates"
  | "advanced"
  | "history";

export interface AgentEditorTab {
  id: AgentEditorTabId;
  label: string;
  description: string;
}

export const AGENT_EDITOR_TABS: readonly AgentEditorTab[] = [
  { id: "basics", label: "Basics", description: "Identity, model, prompt, limits." },
  { id: "tools", label: "Tools", description: "Allowed and excluded tools." },
  { id: "plugins", label: "Plugins", description: "Enabled plugins and their config." },
  { id: "delegates", label: "Delegates", description: "Agents this one can delegate to." },
  { id: "advanced", label: "Advanced", description: "Raw JSON preview." },
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
