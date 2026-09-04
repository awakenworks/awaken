import type { AgentConfig, RuntimeCap } from "./api/types";
import { isAcpModelSelection } from "./agent-model-selection";

export type ToolRealization = "host_executed" | "provider_server";
export type ToolSupport = "supported" | "conditional" | "unavailable";

export interface RuntimeToolCompatibility {
  support: ToolSupport;
  owner: "awaken" | "awaken_bridge" | "model_provider";
  approval: "supported" | "unsupported";
  reason: "native" | "bridge_ready" | "bridge_unverified" | "bridge_missing" | "provider_ready" | "provider_unverified" | "provider_missing";
}

/** One projection shared by authoring and publication review. The model-facing
 * tool id never implies who executes it; realization plus Runtime capability do. */
export function runtimeToolCompatibility(
  model: AgentConfig["model"],
  runtime: RuntimeCap | undefined,
  realization: ToolRealization = "host_executed",
): RuntimeToolCompatibility {
  if (realization === "provider_server") {
    const support = runtime?.features?.provider_server_tools ?? (isAcpModelSelection(model) ? "conditional" : "supported");
    return {
      support,
      owner: "model_provider",
      approval: "unsupported",
      reason: support === "supported" ? "provider_ready" : support === "conditional" ? "provider_unverified" : "provider_missing",
    };
  }
  if (!isAcpModelSelection(model)) {
    return { support: "supported", owner: "awaken", approval: "supported", reason: "native" };
  }
  const support = runtime?.features?.awaken_tool_bridge ?? "conditional";
  return {
    support,
    owner: "awaken_bridge",
    approval: "supported",
    reason: support === "supported" ? "bridge_ready" : support === "conditional" ? "bridge_unverified" : "bridge_missing",
  };
}
