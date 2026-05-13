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
  if (!capabilities || capabilities.tools.length === 0) {
    return (
      <div className="rounded-md border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No tools are currently published. Once plugins or MCP servers register tools, they will
        appear here.
      </div>
    );
  }
  return (
    <>
      <ToolSelector
        title="Allowed Tools"
        description='"All tools" is the default — every published tool is exposed. Switch to Custom to restrict the agent to a specific subset.'
        tools={capabilities.tools}
        value={spec.allowed_tools}
        onChange={(next) => updateField("allowed_tools", next)}
        variant="include"
      />
      <ToolSelector
        title="Excluded Tools"
        description="Excluded tools are removed from the effective allow-list, even if they appear in 'All tools'. Useful for keeping a tool published to other agents but blocking it here."
        tools={capabilities.tools}
        value={spec.excluded_tools}
        onChange={(next) => updateField("excluded_tools", next)}
        variant="exclude"
      />
    </>
  );
}
