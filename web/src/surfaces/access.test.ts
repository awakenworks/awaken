import { describe, expect, it } from "vitest";
import { mintedServiceKey, serviceKeyRoleGuidance } from "./access";

describe("service API key onboarding", () => {
  it("extracts only the one-time secret and never serializes the response", () => {
    expect(mintedServiceKey({
      token: "sk-awaken-once", // awaken-allow(secret): -- inert one-time-key fixture
      api_token: { id: "token-1" },
    })).toEqual({
      id: "token-1",
      secret: "sk-awaken-once", // awaken-allow(secret): -- inert expected fixture
    });
    expect(() => mintedServiceKey({ api_token: { id: "token-1" } }))
      .toThrow("did not return a one-time service API key");
  });

  it("makes the SDK run boundary explicit for every selectable role", () => {
    // C/E table: read-only -> no Session creation; Workspace admin -> SDK
    // creation; admin -> full authority at the minted Workspace binding, not
    // an automatic organization/cross-Workspace grant. Existing principal
    // bindings remain PDP-owned; this is a presentation guard, not a policy.
    expect(serviceKeyRoleGuidance("workspace_restricted_developer", "en")).toContain("cannot create Sessions");
    expect(serviceKeyRoleGuidance("workspace_admin", "en")).toContain("SDK quickstart");
    expect(serviceKeyRoleGuidance("admin", "en")).toContain("in this Workspace");
    expect(serviceKeyRoleGuidance("admin", "en")).toContain("does not grant cross-Workspace or organization-level access");
    expect(serviceKeyRoleGuidance("admin", "zh")).toContain("本身不会授予跨工作区或组织级权限");
    expect(serviceKeyRoleGuidance("workspace_admin", "zh")).toContain("SDK 快速开始");
  });
});
