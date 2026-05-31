import { useMemo } from "react";
import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { ToolSelector } from "@/components/tool-selector";

export function ToolsPanel({
  spec,
  capabilities,
  updateField,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
}) {
  const registered = useMemo(
    () => (capabilities?.tools ?? []).map((t) => t.id),
    [capabilities],
  );

  // Show the loading state ONLY while capabilities are still being
  // fetched. Once `capabilities` resolves with an empty `tools` list, the
  // pattern editors must render so operators can author forward-config
  // (e.g. `excluded_tool_patterns: ["dangerous-*"]`) before tools are
  // registered. `ToolSelector` itself handles `registered.length === 0`
  // gracefully with a "No tools registered." sub-message.
  if (!capabilities) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        Loading published tool capabilities...
      </div>
    );
  }
  return (
    <>
      <ToolSelector
        label="Allowed"
        registered={registered}
        toolDetails={capabilities.tools}
        literals={spec.allowed_tools ?? []}
        patterns={spec.allowed_tool_patterns ?? []}
        onChange={(next) => {
          updateField("allowed_tools", next.literals);
          updateField("allowed_tool_patterns", next.patterns);
        }}
      />
      <ToolSelector
        label="Excluded"
        registered={registered}
        toolDetails={capabilities.tools}
        literals={spec.excluded_tools ?? []}
        patterns={spec.excluded_tool_patterns ?? []}
        onChange={(next) => {
          updateField("excluded_tools", next.literals);
          updateField("excluded_tool_patterns", next.patterns);
        }}
      />
    </>
  );
}
