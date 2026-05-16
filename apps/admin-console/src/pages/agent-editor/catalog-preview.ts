import type { AgentSpec, Capabilities } from "@/lib/config-api";
import { deepEqualCanonical } from "@/lib/agent-editor-canonical";
import { isToolAllowed } from "@/lib/agent-tool-selection";
import { normalizeCatalogForSave } from "./spec-helpers";

export function catalogSpecsEqual(left: AgentSpec, right: AgentSpec): boolean {
  return deepEqualCanonical(normalizeCatalogForSave(left), normalizeCatalogForSave(right));
}

export function catalogFieldsEqual(left: AgentSpec, right: AgentSpec | null): boolean {
  if (!right) return false;
  const normalizedLeft = normalizeCatalogForSave(left);
  const normalizedRight = normalizeCatalogForSave(right);
  return (
    deepEqualCanonical(normalizedLeft.allowed_tools, normalizedRight.allowed_tools) &&
    deepEqualCanonical(normalizedLeft.excluded_tools, normalizedRight.excluded_tools)
  );
}

export function computeCatalogPreviewTools(
  tools: Capabilities["tools"],
  allowed: AgentSpec["allowed_tools"],
  excluded: AgentSpec["excluded_tools"],
): Capabilities["tools"] {
  return tools.filter(
    (tool) =>
      isToolAllowed(allowed, tool.id, "include") &&
      !isToolAllowed(excluded, tool.id, "exclude"),
  );
}
