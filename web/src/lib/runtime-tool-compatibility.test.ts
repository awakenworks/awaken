import { describe, expect, it } from "vitest";
import type { RuntimeCap } from "./api/types";
import { runtimeToolCompatibility } from "./runtime-tool-compatibility";

const acp = (bridge: "supported" | "conditional" | "unavailable", provider = bridge): RuntimeCap => ({
  id: "acp:codex",
  label: "Codex",
  kind: "acp",
  description: "test",
  features: {
    environment_session: "supported",
    context_projection: "supported",
    awaken_tool_bridge: bridge,
    state_machine: "unavailable",
    background_tools: "unavailable",
    working_directory: "supported",
    provider_server_tools: provider,
  },
});

describe("Runtime tool compatibility", () => {
  it("keeps native, bridged, and provider-server execution ownership distinct", () => {
    expect(runtimeToolCompatibility({ mode: "auto" }, undefined)).toMatchObject({ owner: "awaken", support: "supported", approval: "supported" });
    expect(runtimeToolCompatibility({ mode: "backend_default", backend_ref: "acp:codex" }, acp("supported"))).toMatchObject({ owner: "awaken_bridge", support: "supported", approval: "supported" });
    expect(runtimeToolCompatibility({ mode: "backend_default", backend_ref: "acp:codex" }, acp("supported"), "provider_server")).toMatchObject({ owner: "model_provider", support: "supported", approval: "unsupported" });
  });

  it("fails closed when an ACP execution path is absent or unverified", () => {
    expect(runtimeToolCompatibility({ mode: "backend_default", backend_ref: "acp:codex" }, acp("conditional")).support).toBe("conditional");
    expect(runtimeToolCompatibility({ mode: "backend_default", backend_ref: "acp:codex" }, acp("unavailable")).support).toBe("unavailable");
    expect(runtimeToolCompatibility({ mode: "backend_default", backend_ref: "acp:codex" }, undefined).support).toBe("conditional");
  });
});
