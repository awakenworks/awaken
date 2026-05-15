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
  if (!capabilities || capabilities.tools.length === 0) {
    return (
      <div className="rounded-md border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No tools are currently published. Once plugins or MCP servers register tools, they will
        appear here.
      </div>
    );
  }
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
      <ToolSelector
        title="Allowed Tools"
        description='"All tools" is the default — every published tool is exposed. Switch to Custom to restrict the agent to a specific subset.'
        tools={capabilities.tools}
        value={spec.allowed_tools}
        onChange={(next) => updateField("allowed_tools", next)}
        variant="include"
        {...resetProps("allowed_tools")}
      />
      <ToolSelector
        title="Excluded Tools"
        description="Excluded tools are removed from the effective allow-list, even if they appear in 'All tools'. Useful for keeping a tool published to other agents but blocking it here."
        tools={capabilities.tools}
        value={spec.excluded_tools}
        onChange={(next) => updateField("excluded_tools", next)}
        variant="exclude"
        {...resetProps("excluded_tools")}
      />
      {children}
    </div>
  );
}
