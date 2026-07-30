import { describe, expect, it } from "vitest";
import { buildDeferredSandboxPolicy, buildEnvironmentConfig, isolationLabel } from "./environments";

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

describe("buildDeferredSandboxPolicy", () => {
  // UI decision table:
  // U1 cloud/eager and U3 self-hosted/eager -> Environment only, no policy.
  // U2 cloud/on_tool_use -> fail closed before writing a policy.
  // U4 self-hosted/on_tool_use -> exact active v1 policy for later binding.
  it.each([
    ["cloud", "eager"],
    ["self_hosted", "eager"],
  ] as const)("does not create a policy for %s / %s", (placement, provisioning) => {
    expect(buildDeferredSandboxPolicy("env_1", placement, provisioning)).toBeNull();
  });

  it("fails closed if deferred creation is requested for cloud placement", () => {
    expect(() => buildDeferredSandboxPolicy("env_1", "cloud", "on_tool_use")).toThrow(
      /requires a self-hosted native Awaken Environment/,
    );
  });

  it("creates the exact native deferred policy for a self-hosted Environment", () => {
    expect(buildDeferredSandboxPolicy("env_1", "self_hosted", "on_tool_use")).toEqual({
      id: "environment-env_1-sandbox",
      config: {},
      provisioning: "on_tool_use",
      disabled: false,
    });
  });
});

describe("isolationLabel", () => {
  it("shows official environment networking", () => {
    expect(isolationLabel({ type: "cloud", networking: { type: "unrestricted" } })).toBe("unrestricted");
  });
});
