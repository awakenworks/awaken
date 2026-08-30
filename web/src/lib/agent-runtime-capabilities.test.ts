import { describe, expect, it } from "vitest";
import { runtimeCapabilitySupport } from "./agent-runtime-capabilities";

describe("Agent execution capability matrix", () => {
  it("keeps Managed Agent platform capabilities independent from the selected Harness", () => {
    const native = runtimeCapabilitySupport(false);
    const acp = runtimeCapabilitySupport(true);
    expect(native.environment_session).toBe("supported");
    expect(acp.environment_session).toBe("supported");
    expect(native.context).toBe("supported");
    expect(acp.context).toBe("supported");
  });

  it("narrows only capabilities that cross the external Harness boundary", () => {
    const native = runtimeCapabilitySupport(false);
    const acp = runtimeCapabilitySupport(true);
    expect(native.tools_mcp).toBe("supported");
    expect(acp.tools_mcp).toBe("conditional");
    expect(native.state_concurrency).toBe("supported");
    expect(acp.state_concurrency).toBe("conditional");
    expect(native.background_tools).toBe("supported");
    expect(acp.background_tools).toBe("unavailable");
  });
});
