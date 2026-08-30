import { describe, expect, it } from "vitest";
import { mintedServiceKey, serviceKeyRoleGuidance } from "./access";

describe("service API key onboarding", () => {
  it("extracts only the one-time secret and never serializes the response", () => {
    expect(mintedServiceKey({ token: "sk-awaken-once", api_token: { id: "token-1" } }))
      .toEqual({ id: "token-1", secret: "sk-awaken-once" });
    expect(() => mintedServiceKey({ api_token: { id: "token-1" } }))
      .toThrow("did not return a one-time service API key");
  });

  it("makes the SDK run boundary explicit for every selectable role", () => {
    expect(serviceKeyRoleGuidance("workspace_restricted_developer", "en")).toContain("cannot create Sessions");
    expect(serviceKeyRoleGuidance("workspace_admin", "en")).toContain("SDK quickstart");
    expect(serviceKeyRoleGuidance("admin", "en")).toContain("cross-Workspace");
    expect(serviceKeyRoleGuidance("workspace_admin", "zh")).toContain("SDK 快速开始");
  });
});
