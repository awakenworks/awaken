import { type ReactNode } from "react";
import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { ToolSelector } from "@/components/tool-selector";

export function ToolsPanel({
  spec,
  capabilities,
  updateField,
  canResetFields,
  overriddenFields,
  onResetField,
  children,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  canResetFields?: boolean;
  overriddenFields?: Set<string>;
  onResetField?: (field: string) => void;
  children?: ReactNode;
}) {
  const tools = capabilities?.tools ?? [];
  const resetProps = (field: "allowed_tools" | "excluded_tools") => {
    if (!canResetFields || !overriddenFields?.has(field) || !onResetField) {
      return {};
    }
    return {
      overridden: true,
      onReset: () => onResetField(field),
      resetLabel: `Reset ${field} to default`,
    } as const;
  };
  return (
    <div className="space-y-6">
      {tools.length === 0 ? (
        <div className="rounded-md border border-dashed border-line bg-surface p-4 text-sm text-fg-soft">
          No tools are currently published.
        </div>
      ) : null}
      <ToolSelector
        title="Allowed Tools"
        description='"All tools" is the default — every published tool is exposed. Switch to Custom to restrict the agent to a specific subset.'
        tools={tools}
        value={spec.allowed_tools}
        onChange={(next) => updateField("allowed_tools", next)}
        variant="include"
        {...resetProps("allowed_tools")}
      />
      <ToolSelector
        title="Excluded Tools"
        description="Excluded tools are removed from the effective allow-list, even if they appear in 'All tools'. Useful for keeping a tool published to other agents but blocking it here."
        tools={tools}
        value={spec.excluded_tools}
        onChange={(next) => updateField("excluded_tools", next)}
        variant="exclude"
        {...resetProps("excluded_tools")}
      />
      {children}
    </div>
  );
}
