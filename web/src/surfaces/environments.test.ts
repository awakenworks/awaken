import { describe, expect, it } from "vitest";
import { buildEnvironmentConfig, isolationLabel } from "./environments";

describe("buildEnvironmentConfig", () => {
  it("emits only the official cloud union fields", () => {
    expect(buildEnvironmentConfig("cloud", "limited", "api.example.com, *.example.org")).toEqual({
      type: "cloud",
      networking: {
        type: "limited",
        allowed_hosts: ["api.example.com", "*.example.org"],
        allow_mcp_servers: true,
      },
    });
  });

  it("emits the exact self-hosted variant without private runtime or sandbox fields", () => {
    expect(buildEnvironmentConfig("self_hosted", "limited", "ignored.example")).toEqual({
      type: "self_hosted",
    });
  });
});

describe("isolationLabel", () => {
  it("shows official environment networking", () => {
    expect(isolationLabel({ type: "cloud", networking: { type: "unrestricted" } })).toBe("unrestricted");
  });
});
