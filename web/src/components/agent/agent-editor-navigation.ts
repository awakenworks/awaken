export type AuthorStage = "quickstart" | "build" | "advanced";
export type BuilderSection = "instructions" | "tools" | "integrations" | "knowledge";
export type AdvancedSection = "orchestration" | "extensions" | "source" | "release";

export function stageForPath(path: string): AuthorStage {
  const top = path.split(".")[0];
  if (["multiagent", "delegation_limits", "metadata", "compaction"].includes(top)) {
    return "advanced";
  }
  if (top === "plugin_config") {
    const section = path.split(".")[1] ?? "";
    return ["permission", "compact", "memory", "web_search"].includes(section)
      ? "build"
      : "advanced";
  }
  return "build";
}

export function builderSectionForPath(path: string): BuilderSection {
  const top = path.split(".")[0];
  if (top === "tools" || top === "tool_overrides" || top === "tool_patterns" || top === "recovery_policies" || path.includes("permission")) return "tools";
  if (top === "mcp_servers" || top === "skills") return "integrations";
  if (top === "resources" || path.includes("memory")) return "knowledge";
  return "instructions";
}

export function advancedSectionForPath(path: string): AdvancedSection {
  const top = path.split(".")[0];
  if (top === "multiagent" || top === "delegation_limits" || top === "metadata" || path.includes("state_machine")) {
    return "orchestration";
  }
  if (top === "compaction" || top === "plugin_config") {
    return "extensions";
  }
  return "source";
}
